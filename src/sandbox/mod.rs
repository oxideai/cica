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

/// Where attachments live, relative to the workspace root.
///
/// The prompt names an attachment by this path and the worker hydrates it to the
/// same place. They must agree, so both read it from here.
pub const ATTACHMENTS_SUBDIR: &str = "internal/slack_attachments";

/// The path to quote in a prompt for `name` — relative, because the machine that
/// writes the prompt is often not the machine that opens the file.
pub fn attachment_path(name: &str) -> String {
    format!("{ATTACHMENTS_SUBDIR}/{name}")
}

/// A single agent turn to execute.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnJob {
    pub channel: String,
    pub user_id: String,
    /// The user/cron prompt to send to the agent.
    pub prompt: String,
    /// System prompt (full on new session, appended on resume — backend decides).
    pub system_prompt: Option<String>,
    /// Backend session id to resume, if any.
    pub resume_session: Option<String>,
    pub skip_permissions: bool,
    pub backend: AiBackend,
    /// Model alias or full id selected by the router.
    pub model: Option<String>,
    /// File names of Slack attachments this turn refers to, relative to
    /// `internal/slack_attachments/`.
    ///
    /// The router downloads attachments to its own disk, so a worker on another
    /// machine cannot open them. Naming them here lets the dispatcher copy each
    /// one into the state store and the worker pull it back before the turn.
    /// Defaulted so jobs written by an older router still deserialize.
    #[serde(default)]
    pub attachments: Vec<String>,
}

impl TurnJob {
    /// The router's turn contract: backend and model are decided here, from the router's
    /// config, and the worker honours them regardless of its own environment.
    pub fn new(
        config: &Config,
        channel: &str,
        user_id: &str,
        prompt: String,
        system_prompt: Option<String>,
        resume_session: Option<String>,
    ) -> Self {
        Self {
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            prompt,
            system_prompt,
            resume_session,
            skip_permissions: true,
            backend: config.backend,
            model: config.model_for(config.backend),
            attachments: Vec::new(),
        }
    }

    /// Name the Slack attachments this turn refers to, so they travel with it.
    pub fn with_attachments(mut self, attachments: Vec<String>) -> Self {
        self.attachments = attachments;
        self
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnResult {
    pub response: String,
    /// Backend-assigned session id for the resulting conversation.
    pub backend_session_id: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// File names the agent produced and named with an `[attachment:...]`
    /// marker, relative to the turn's `out/` prefix in the store.
    ///
    /// A worker writes them inside a container the router cannot see, so they
    /// travel through the store the same way inbound attachments do. Defaulted
    /// so a result written by an older worker still deserializes.
    #[serde(default)]
    pub produced_files: Vec<String>,
}

/// Paths named by `[attachment:/path/to/file]` markers, in the order they
/// appear, without checking whether any of them exist.
///
/// The check has to happen on whichever machine holds the file: the worker that
/// wrote it, or the router after pulling it. Sharing the parser keeps those two
/// readings of the same marker from drifting apart.
pub fn attachment_markers(text: &str) -> Vec<String> {
    const OPEN: &str = "[attachment:";
    let mut out = Vec::new();
    for (idx, _) in text.match_indices(OPEN) {
        let start = idx + OPEN.len();
        let Some(end) = text[start..].find(']') else {
            continue;
        };
        let path = text[start..start + end].trim();
        if !path.is_empty() && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
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
                paths.internal_dir.join("slack_attachments"),
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
                config.skills.is_none().then(|| paths.skills_dir.clone()),
                state_store_dir,
            );
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(launcher),
                paths.internal_dir.join("slack_attachments"),
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
                    paths.internal_dir.join("slack_attachments"),
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
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            skip_permissions: true,
            backend: crate::config::AiBackend::Claude,
            model: None,
            attachments: Vec::new(),
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: TurnJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.channel, "telegram");
        assert_eq!(back.resume_session.as_deref(), Some("sess-1"));

        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess-2".into(),
            cost_usd: Some(0.1),
            duration_ms: Some(5),
            produced_files: Vec::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TurnResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend_session_id, "sess-2");
    }

    #[test]
    fn new_job_takes_backend_and_model_from_config() {
        let mut cfg = Config {
            backend: AiBackend::Claude,
            ..Default::default()
        };
        cfg.claude.model = Some("opus".into());
        cfg.cursor.model = Some("auto".into());
        let job = TurnJob::new(&cfg, "telegram", "1", "hi".into(), None, None);
        assert_eq!(job.backend, AiBackend::Claude);
        assert_eq!(job.model.as_deref(), Some("opus"));
        assert!(job.skip_permissions);

        cfg.backend = AiBackend::Cursor;
        let job = TurnJob::new(&cfg, "telegram", "1", "hi".into(), None, None);
        assert_eq!(job.backend, AiBackend::Cursor);
        assert_eq!(job.model.as_deref(), Some("auto"));
        assert!(job.skip_permissions);
    }

    #[test]
    fn turn_job_ignores_fields_an_older_router_sends() {
        let json = r#"{
            "session_id":"telegram:1","channel":"telegram","user_id":"1",
            "prompt":"hi","system_prompt":null,"resume_session":"sess-1",
            "cwd":"/tmp/work","skip_permissions":true,"backend":"claude","model":null
        }"#;
        let job: TurnJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.channel, "telegram");
        assert_eq!(job.resume_session.as_deref(), Some("sess-1"));
    }
}

#[cfg(test)]
mod attachment_compat_tests {
    use super::*;

    #[test]
    fn a_job_written_by_an_older_router_still_deserializes() {
        // Jobs already sitting in the state store predate the field.
        let old = r#"{"channel":"slack","user_id":"U1","prompt":"hi","system_prompt":null,
                      "resume_session":null,"skip_permissions":true,"backend":"claude","model":null}"#;
        let job: TurnJob = serde_json::from_str(old).expect("old job deserializes");
        assert!(job.attachments.is_empty());
    }

    #[test]
    fn attachments_survive_a_round_trip() {
        let old = r#"{"channel":"slack","user_id":"U1","prompt":"hi","system_prompt":null,
                      "resume_session":null,"skip_permissions":true,"backend":"claude","model":null}"#;
        let job: TurnJob = serde_json::from_str(old).unwrap();
        let job = job.with_attachments(vec!["F1_shot.png".into()]);
        let back: TurnJob = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        assert_eq!(back.attachments, vec!["F1_shot.png".to_string()]);
    }

    #[test]
    fn the_prompt_path_and_the_hydration_dir_agree() {
        assert_eq!(
            attachment_path("x.png"),
            format!("{ATTACHMENTS_SUBDIR}/x.png")
        );
    }
}
