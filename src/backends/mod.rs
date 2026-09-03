//! AI Backend abstraction for Claude Code and Cursor CLI

pub mod claude;
pub mod cursor;

use anyhow::Result;

use crate::config::{AiBackend, Config};

#[derive(Default)]
pub struct QueryOptions {
    pub system_prompt: Option<String>,
    pub resume_session: Option<String>,
    pub cwd: Option<String>,
    pub skip_permissions: bool,
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

pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<QueryResult> {
    // Test hook: a deterministic response without invoking the real backend CLI.
    // Inert unless `CICA_FAKE_BACKEND` is set (used only by the Docker CI test).
    if std::env::var_os("CICA_FAKE_BACKEND").is_some() {
        return Ok(fake_result(prompt));
    }

    let config = Config::load()?;

    match config.backend {
        AiBackend::Claude => query_claude(prompt, options, &config).await,
        AiBackend::Cursor => query_cursor(prompt, options, &config).await,
    }
}

async fn query_claude(prompt: &str, options: QueryOptions, config: &Config) -> Result<QueryResult> {
    let claude_options = claude::QueryOptions {
        system_prompt: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        skip_permissions: options.skip_permissions,
        model: config.claude.model.clone(),
    };

    claude::query_with_options(prompt, claude_options).await
}

async fn query_cursor(prompt: &str, options: QueryOptions, config: &Config) -> Result<QueryResult> {
    let cursor_options = cursor::QueryOptions {
        context: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        force: options.skip_permissions,
        model: config.cursor.model.clone(),
    };

    cursor::query_with_options(prompt, cursor_options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_result_echoes_prompt() {
        let r = fake_result("ping");
        assert_eq!(r.response, "fake-response: ping");
        assert_eq!(r.session_id, "");
        assert_eq!(r.cost_usd, None);
    }
}
