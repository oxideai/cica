//! Sandbox abstraction: where an agent turn executes.
//!
//! Phase 1 provides only `LocalProcessProvider`, which runs the agent as a
//! local subprocess (today's behavior). Later phases add container-based
//! providers behind the same `SandboxProvider` trait.

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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
/// Phase 1 always returns the local provider; later phases branch on config.
pub fn default_provider(_config: &Config) -> Box<dyn SandboxProvider> {
    Box::new(LocalProcessProvider::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_provider_is_constructible() {
        let cfg = Config::default();
        let _p = default_provider(&cfg);
    }
}
