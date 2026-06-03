//! Sandbox abstraction: where an agent turn executes.
//!
//! Phase 1 provides only `LocalProcessProvider`, which runs the agent as a
//! local subprocess (today's behavior). Later phases add container-based
//! providers behind the same `SandboxProvider` trait.

pub mod artifacts;
pub mod hydrating;
mod local;
pub mod state;

pub use local::{LocalProcessProvider, query_result_from_turn};

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{AiBackend, Config};

/// A single agent turn to execute.
///
/// Some fields (`session_id`, `channel`, `user_id`, `backend`, `model`) are set
/// by callers but not yet read by `LocalProcessProvider`; they're part of the
/// turn contract for later phases (remote workers, per-job backend routing).
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnJob {
    /// Logical cica session key (e.g. "telegram:123").
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    /// The user/cron prompt to send to the agent.
    pub prompt: String,
    /// System prompt (full on new session, appended on resume — backend decides).
    pub system_prompt: Option<String>,
    /// Backend session id to resume, if any.
    pub resume_session: Option<String>,
    pub cwd: Option<String>,
    pub skip_permissions: bool,
    pub backend: AiBackend,
    pub model: Option<String>,
}

/// Result of executing a `TurnJob`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnResult {
    pub response: String,
    /// Backend-assigned session id for the resulting conversation.
    pub backend_session_id: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
}

/// Where an agent turn executes.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult>;
}

/// Build the provider selected by configuration.
///
/// Returns a `HydratingProvider` when a state store is configured, otherwise
/// falls back to `LocalProcessProvider` (same behavior as Phase 1).
pub fn default_provider(config: &Config) -> Box<dyn SandboxProvider> {
    let local = LocalProcessProvider::new();
    match state::default_store(config) {
        Ok(Some(store)) => match crate::config::paths() {
            Ok(paths) => Box::new(hydrating::HydratingProvider::new(
                local,
                store,
                paths.claude_home,
                paths.base,
            )),
            Err(e) => {
                tracing::warn!(
                    "state store configured but paths unavailable ({e}); running without hydration"
                );
                Box::new(LocalProcessProvider::new())
            }
        },
        Ok(None) => Box::new(local),
        Err(e) => {
            tracing::warn!("failed to build state store ({e}); running without hydration");
            Box::new(local)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_provider_is_constructible() {
        let cfg = Config::default();
        let _p = default_provider(&cfg);
    }

    #[test]
    fn turn_job_and_result_round_trip_json() {
        let job = TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            cwd: None,
            skip_permissions: true,
            backend: crate::config::AiBackend::Claude,
            model: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: TurnJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "telegram:1");
        assert_eq!(back.resume_session.as_deref(), Some("sess-1"));

        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess-2".into(),
            cost_usd: Some(0.1),
            duration_ms: Some(5),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TurnResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend_session_id, "sess-2");
    }
}
