//! Sandbox abstraction: where an agent turn executes.
//!
//! Phase 1 provides only `LocalProcessProvider`, which runs the agent as a
//! local subprocess (today's behavior). Later phases add container-based
//! providers behind the same `SandboxProvider` trait.

pub mod artifacts;
#[cfg(feature = "fargate")]
mod fargate;
pub mod hydrating;
mod local;
pub mod state;
pub mod worker;

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

/// Build the configured provider. Errors when the configuration is invalid
/// (e.g. `provider = subprocess` without a store).
pub fn try_default_provider(config: &Config) -> Result<Box<dyn SandboxProvider>> {
    use crate::config::ProviderKind;

    let store = state::default_store(config)?;

    match config.deployment.provider.unwrap_or(ProviderKind::Local) {
        ProviderKind::Local => {
            let local = LocalProcessProvider::new();
            match store {
                Some(store) => {
                    let paths = crate::config::paths()?;
                    Ok(Box::new(hydrating::HydratingProvider::new(
                        local,
                        store,
                        paths.claude_home,
                        paths.cursor_home,
                        paths.base,
                    )))
                }
                None => Ok(Box::new(local)),
            }
        }
        ProviderKind::Subprocess => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = subprocess` requires [deployment].store to be set")
            })?;
            let self_exe = std::env::current_exe()?;
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(worker::SubprocessLauncher::new(self_exe)),
            )))
        }
        ProviderKind::Docker => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = docker` requires [deployment].store to be set")
            })?;
            let paths = crate::config::paths()?;
            let image = config
                .deployment
                .docker_image
                .clone()
                .unwrap_or_else(|| "cica-worker:latest".to_string());
            let state_store_dir = state::resolved_state_path(config)?;
            let launcher = worker::DockerLauncher::new(
                image,
                paths.config_file,
                paths.skills_dir,
                state_store_dir,
            );
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(launcher),
            )))
        }
    }
}

/// Infallible wrapper used by call sites that cannot recover. On a
/// configuration error it logs and falls back to the in-process provider so
/// the router still starts (per the spec's "router-still-starts" choice).
/// Note the trade-off: a misconfigured `provider = subprocess` (e.g. missing
/// store) silently runs in-process instead of dispatching to a worker.
pub fn default_provider(config: &Config) -> Box<dyn SandboxProvider> {
    match try_default_provider(config) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("invalid provider configuration ({e}); using in-process provider");
            Box::new(LocalProcessProvider::new())
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
    fn subprocess_provider_requires_a_store() {
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        // No store configured → must be an error, not a silent local fallback.
        assert!(try_default_provider(&cfg).is_err());
    }

    #[test]
    fn subprocess_provider_built_when_store_present() {
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-prov-test".into());
        assert!(try_default_provider(&cfg).is_ok());
    }

    #[test]
    fn docker_provider_requires_a_store() {
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        assert!(try_default_provider(&cfg).is_err());
    }

    #[test]
    fn docker_provider_built_when_store_present() {
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-docker-test".into());
        assert!(try_default_provider(&cfg).is_ok());
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
