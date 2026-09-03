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

use crate::config::{AiBackend, Config, Paths};

/// A single agent turn to execute.
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnResult {
    pub response: String,
    /// Backend-assigned session id for the resulting conversation.
    pub backend_session_id: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
}

#[async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult>;
}

/// Build the configured provider. Errors when the configuration is invalid
/// (e.g. `provider = subprocess` without a store).
pub fn try_default_provider(config: &Config, paths: &Paths) -> Result<Box<dyn SandboxProvider>> {
    use crate::config::ProviderKind;

    let store = state::default_store(config, paths)?;

    match config.deployment.provider.unwrap_or(ProviderKind::Local) {
        ProviderKind::Local => {
            let local = LocalProcessProvider::new(config.clone(), paths.clone());
            match store {
                Some(store) => Ok(Box::new(hydrating::HydratingProvider::new(
                    local,
                    store,
                    paths.claude_home.clone(),
                    paths.cursor_home.clone(),
                    paths.base.clone(),
                ))),
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
            let image = config
                .deployment
                .docker_image
                .clone()
                .unwrap_or_else(|| "cica-worker:latest".to_string());
            let state_store_dir = state::resolved_state_path(config, paths);
            let launcher = worker::DockerLauncher::new(
                image,
                paths.config_file.clone(),
                paths.skills_dir.clone(),
                state_store_dir,
            );
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(launcher),
            )))
        }
        ProviderKind::Fargate => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = fargate` requires [deployment].store to be set")
            })?;
            #[cfg(feature = "fargate")]
            {
                let fc = config.deployment.fargate.clone().ok_or_else(|| {
                    anyhow::anyhow!("`provider = fargate` requires a [deployment.fargate] section")
                })?;
                Ok(Box::new(worker::LaunchedWorkerProvider::new(
                    store,
                    Box::new(fargate::FargateLauncher::new(fc)),
                )))
            }
            #[cfg(not(feature = "fargate"))]
            {
                let _ = store;
                anyhow::bail!(
                    "`provider = fargate` requires the binary to be built with `--features fargate`"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        // No store configured → must be an error, not a silent local fallback.
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn subprocess_provider_built_when_store_present() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-prov-test".into());
        assert!(try_default_provider(&cfg, &paths).is_ok());
    }

    #[test]
    fn docker_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn docker_provider_built_when_store_present() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-docker-test".into());
        assert!(try_default_provider(&cfg, &paths).is_ok());
    }

    #[cfg(not(feature = "fargate"))]
    #[test]
    fn fargate_provider_requires_feature() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-fargate-test".into());
        // Feature off → must error even though a store is present.
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[test]
    fn fargate_provider_requires_a_store() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        assert!(try_default_provider(&cfg, &paths).is_err());
    }

    #[cfg(feature = "fargate")]
    #[test]
    fn fargate_provider_built_when_feature_and_store_and_section() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::{Config, FargateConfig, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-fargate-test2".into());
        cfg.deployment.fargate = Some(FargateConfig {
            cluster: "cica".into(),
            task_definition: "cica-worker".into(),
            ..Default::default()
        });
        // Lazy ECS client: building the provider does not connect.
        assert!(try_default_provider(&cfg, &paths).is_ok());
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
