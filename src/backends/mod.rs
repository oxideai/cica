//! AI Backend abstraction for Claude Code and Cursor CLI

pub mod claude;
pub mod cursor;

use anyhow::Result;

use crate::config::{AiBackend, Config, Paths};

type Killer = fn(i32, i32) -> i32;

pub(crate) struct ProcessGroupGuard {
    pid: Option<i32>,
    killer: Killer,
}

impl ProcessGroupGuard {
    pub(crate) fn new(pid: u32) -> Self {
        Self {
            pid: Some(pid as i32),
            killer: |pid, signal| unsafe { libc::kill(pid, signal) },
        }
    }

    #[cfg(test)]
    fn with_killer(pid: u32, killer: Killer) -> Self {
        Self {
            pid: Some(pid as i32),
            killer,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            (self.killer)(-pid, libc::SIGKILL);
        }
    }
}

#[derive(Default)]
pub struct QueryOptions {
    pub system_prompt: Option<String>,
    pub resume_session: Option<String>,
    pub skip_permissions: bool,
    /// Alias or full id. `None` = the backend CLI's own default.
    pub model: Option<String>,
}

/// Result returned by all AI backends.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub response: String,
    pub session_id: String,
    pub duration_ms: Option<u64>,
    /// Cost of this query in USD (backend-dependent; Claude provides this, Cursor does not).
    pub cost_usd: Option<f64>,
}

/// Deterministic stand-in for a real backend response. Used by the Docker
/// integration test (activated via the `CICA_FAKE_BACKEND` env var) to exercise
/// the worker/dispatch pipeline without calling Cursor/Claude.
fn fake_result(prompt: &str) -> QueryResult {
    QueryResult {
        response: format!("fake-response: {prompt}"),
        session_id: String::new(),
        duration_ms: Some(0),
        cost_usd: None,
    }
}

pub async fn query_with_options(
    backend: AiBackend,
    config: &Config,
    paths: &Paths,
    prompt: &str,
    mut options: QueryOptions,
) -> Result<QueryResult> {
    // Test hook: a deterministic response without invoking the real backend CLI.
    // Inert unless `CICA_FAKE_BACKEND` is set (used only by the Docker CI test).
    if std::env::var_os("CICA_FAKE_BACKEND").is_some() {
        return Ok(fake_result(prompt));
    }

    options.model = effective_model(options.model, backend, config);
    match backend {
        AiBackend::Claude => {
            claude::query_with_options(&config.claude, paths, prompt, options).await
        }
        AiBackend::Cursor => {
            cursor::query_with_options(&config.cursor, paths, prompt, options).await
        }
    }
}

fn effective_model(
    requested: Option<String>,
    backend: AiBackend,
    config: &Config,
) -> Option<String> {
    requested.or_else(|| config.model_for(backend))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static KILLED_PID: AtomicI32 = AtomicI32::new(0);

    fn record_kill(pid: i32, _signal: i32) -> i32 {
        KILLED_PID.store(pid, Ordering::SeqCst);
        0
    }

    #[test]
    fn process_group_guard_signals_negative_pid() {
        KILLED_PID.store(0, Ordering::SeqCst);
        drop(ProcessGroupGuard::with_killer(42, record_kill));
        assert_eq!(KILLED_PID.load(Ordering::SeqCst), -42);
    }

    #[test]
    fn fake_result_echoes_prompt() {
        let r = fake_result("ping");
        assert_eq!(r.response, "fake-response: ping");
        assert_eq!(r.session_id, "");
        assert_eq!(r.cost_usd, None);
    }

    #[test]
    fn effective_model_prefers_the_job() {
        let mut cfg = Config::default();
        cfg.claude.model = Some("configured".into());
        assert_eq!(
            effective_model(Some("requested".into()), AiBackend::Claude, &cfg).as_deref(),
            Some("requested")
        );
    }

    #[test]
    fn effective_model_falls_back_per_backend() {
        let mut cfg = Config::default();
        cfg.claude.model = Some("claude-model".into());
        cfg.cursor.model = Some("cursor-model".into());
        assert_eq!(
            effective_model(None, AiBackend::Claude, &cfg).as_deref(),
            Some("claude-model")
        );
        assert_eq!(
            effective_model(None, AiBackend::Cursor, &cfg).as_deref(),
            Some("cursor-model")
        );
        cfg.claude.model = None;
        cfg.cursor.model = None;
        assert_eq!(effective_model(None, AiBackend::Claude, &cfg), None);
        assert_eq!(effective_model(None, AiBackend::Cursor, &cfg), None);
    }
}
