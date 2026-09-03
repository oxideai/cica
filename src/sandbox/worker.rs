//! Worker dispatch: run a turn in a one-shot `cica worker` child process,
//! exchanging the job and result through the `StateStore` keyed by a turn id.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use uuid::Uuid;

use crate::sandbox::state::StateStore;
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

fn job_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/job")
}

fn result_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/result")
}

fn turn_prefix(turn_id: &str) -> String {
    format!("turns/{turn_id}")
}

fn scratch_dir(turn_id: &str, kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "cica-turn-{turn_id}-{kind}-{}",
        uuid::Uuid::new_v4()
    ))
}

async fn push_job(store: &dyn StateStore, turn_id: &str, job: &TurnJob) -> Result<()> {
    let dir = scratch_dir(turn_id, "job");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("job.json"), serde_json::to_vec_pretty(job)?)?;
    store.push(&dir, &job_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

async fn pull_job(store: &dyn StateStore, turn_id: &str) -> Result<TurnJob> {
    let dir = scratch_dir(turn_id, "job-in");
    let found = store.pull(&job_key(turn_id), &dir).await?;
    let result = if found {
        let bytes = std::fs::read(dir.join("job.json")).context("reading job.json")?;
        serde_json::from_slice(&bytes).context("deserializing TurnJob")
    } else {
        Err(anyhow::anyhow!("no job found for turn {turn_id}"))
    };
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn push_result(store: &dyn StateStore, turn_id: &str, result: &TurnResult) -> Result<()> {
    let dir = scratch_dir(turn_id, "result");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("result.json"), serde_json::to_vec_pretty(result)?)?;
    store.push(&dir, &result_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// `None` if the worker never wrote a result.
async fn pull_result(store: &dyn StateStore, turn_id: &str) -> Result<Option<TurnResult>> {
    let dir = scratch_dir(turn_id, "result-in");
    let found = store.pull(&result_key(turn_id), &dir).await?;
    let result = if found {
        let bytes = std::fs::read(dir.join("result.json")).context("reading result.json")?;
        Some(serde_json::from_slice(&bytes).context("deserializing TurnResult")?)
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    Ok(result)
}

/// Best-effort removal of a turn's blobs after the router has the result.
/// `StateStore` has no delete; pushing an empty dir collapses the subtree.
async fn cleanup(store: &dyn StateStore, turn_id: &str) {
    let empty = scratch_dir(turn_id, "empty");
    if std::fs::create_dir_all(&empty).is_ok() {
        let _ = store.push(&empty, &turn_prefix(turn_id)).await;
        let _ = std::fs::remove_dir_all(&empty);
    }
}

pub async fn run_worker_turn(
    store: &dyn StateStore,
    engine: &dyn crate::sandbox::SandboxProvider,
    turn_id: &str,
) -> Result<()> {
    let job = pull_job(store, turn_id).await?;
    let result = engine.run_turn(job).await?;
    push_result(store, turn_id, &result).await?;
    Ok(())
}

/// Runs the worker for a `turn_id` to completion. `Ok` = clean exit 0;
/// `Err` = launch failure or non-zero exit. Job/result travel via the store.
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, turn_id: &str) -> Result<()>;
}

/// Router-side provider: store-mediated dispatch, delegating the run-to-exit
/// step to a `Launcher` (subprocess, docker, …).
pub struct LaunchedWorkerProvider {
    store: Arc<dyn StateStore>,
    launcher: Box<dyn Launcher>,
}

impl LaunchedWorkerProvider {
    pub fn new(store: Arc<dyn StateStore>, launcher: Box<dyn Launcher>) -> Self {
        Self { store, launcher }
    }
}

#[async_trait]
impl SandboxProvider for LaunchedWorkerProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let turn_id = Uuid::new_v4().to_string();

        push_job(self.store.as_ref(), &turn_id, &job).await?;

        if let Err(e) = self.launcher.launch(&turn_id).await {
            cleanup(self.store.as_ref(), &turn_id).await;
            return Err(e);
        }

        let result = pull_result(self.store.as_ref(), &turn_id).await;
        cleanup(self.store.as_ref(), &turn_id).await;

        result?.ok_or_else(|| anyhow::anyhow!("worker produced no result for turn {turn_id}"))
    }
}

/// Launcher that spawns `cica worker --turn <id>` as a local child process.
pub struct SubprocessLauncher {
    self_exe: PathBuf,
}

impl SubprocessLauncher {
    pub fn new(self_exe: PathBuf) -> Self {
        Self { self_exe }
    }
}

#[async_trait]
impl Launcher for SubprocessLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let status = Command::new(&self.self_exe)
            .arg("worker")
            .arg("--turn")
            .arg(turn_id)
            .kill_on_drop(true)
            .status()
            .await
            .context("spawning cica worker")?;
        if !status.success() {
            anyhow::bail!("worker exited with status {status}");
        }
        Ok(())
    }
}

/// Launcher that runs `cica worker --turn <id>` inside a one-shot container.
///
/// Mounts the host config, published skills, and filesystem state-store into a
/// `/data/cica`-pinned container (the image sets `XDG_CONFIG_HOME=/data`).
/// `cursor-home`/`claude-home` stay container-local (fresh per turn).
pub struct DockerLauncher {
    image: String,
    config_file: PathBuf,
    skills_dir: PathBuf,
    state_store_dir: PathBuf,
    env: Vec<(String, String)>,
}

struct DockerContainerGuard(Option<String>);

impl DockerContainerGuard {
    fn new(name: String) -> Self {
        Self(Some(name))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for DockerContainerGuard {
    fn drop(&mut self) {
        if let Some(name) = self.0.take() {
            tokio::spawn(async move {
                let _ = Command::new("docker").args(["kill", &name]).output().await;
            });
        }
    }
}

impl DockerLauncher {
    pub fn new(
        image: String,
        config_file: PathBuf,
        skills_dir: PathBuf,
        state_store_dir: PathBuf,
    ) -> Self {
        Self {
            image,
            config_file,
            skills_dir,
            state_store_dir,
            env: Vec::new(),
        }
    }

    /// Extra `-e KEY=VALUE` env vars to pass into the container.
    #[cfg(test)]
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// The `docker` argv (without the leading `docker`). Pure, for testing.
    fn run_args(&self, turn_id: &str) -> Vec<String> {
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            format!("cica-turn-{turn_id}"),
        ];
        for (k, v) in &self.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push("-v".into());
        args.push(format!(
            "{}:/data/cica/config.toml:ro",
            self.config_file.display()
        ));
        args.push("-v".into());
        args.push(format!(
            "{}:/data/cica/skills:ro",
            self.skills_dir.display()
        ));
        args.push("-v".into());
        args.push(format!(
            "{}:/data/cica/internal/state-store",
            self.state_store_dir.display()
        ));
        args.push(self.image.clone());
        args.push("worker".into());
        args.push("--turn".into());
        args.push(turn_id.into());
        args
    }
}

#[async_trait]
impl Launcher for DockerLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let mut guard = DockerContainerGuard::new(format!("cica-turn-{turn_id}"));
        let status = Command::new("docker")
            .args(self.run_args(turn_id))
            .kill_on_drop(true)
            .status()
            .await;
        guard.disarm();
        let status = status.context("running `docker run` for cica worker")?;
        if !status.success() {
            anyhow::bail!("worker container exited with status {status}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiBackend;
    use crate::sandbox::state::FilesystemStateStore;

    fn sample_job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
        }
    }

    #[tokio::test]
    async fn job_push_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        push_job(&store, "t1", &sample_job()).await.unwrap();
        let back = pull_job(&store, "t1").await.unwrap();
        assert_eq!(back.channel, "telegram");
        assert_eq!(back.prompt, "hi");
    }

    #[tokio::test]
    async fn pull_result_none_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        assert!(pull_result(&store, "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn result_push_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess".into(),
            cost_usd: None,
            duration_ms: None,
        };
        push_result(&store, "t2", &result).await.unwrap();
        let back = pull_result(&store, "t2").await.unwrap().unwrap();
        assert_eq!(back.backend_session_id, "sess");
    }

    #[tokio::test]
    async fn launched_provider_dispatches_via_launcher() {
        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(root.path().to_path_buf()));

        struct FakeLauncher {
            store: std::sync::Arc<FilesystemStateStore>,
        }
        struct DispatchStubEngine;
        #[async_trait]
        impl SandboxProvider for DispatchStubEngine {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                Ok(TurnResult {
                    response: "ok".into(),
                    backend_session_id: "sess".into(),
                    cost_usd: None,
                    duration_ms: None,
                })
            }
        }
        #[async_trait]
        impl Launcher for FakeLauncher {
            async fn launch(&self, turn_id: &str) -> Result<()> {
                run_worker_turn(self.store.as_ref(), &DispatchStubEngine, turn_id).await
            }
        }

        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(FakeLauncher {
                store: store.clone(),
            }),
        );
        let job = TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
        };
        let result = provider.run_turn(job).await.unwrap();
        assert_eq!(result.backend_session_id, "sess");
    }

    #[test]
    fn docker_launcher_builds_run_args() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/host/config.toml"),
            std::path::PathBuf::from("/host/skills"),
            std::path::PathBuf::from("/host/state-store"),
        );
        let args = l.run_args("turn-123");
        assert_eq!(&args[..4], ["run", "--rm", "--name", "cica-turn-turn-123"]);
        assert!(args.contains(&"/host/config.toml:/data/cica/config.toml:ro".to_string()));
        assert!(args.contains(&"/host/skills:/data/cica/skills:ro".to_string()));
        assert!(args.contains(&"/host/state-store:/data/cica/internal/state-store".to_string()));
        let tail = &args[args.len() - 4..];
        assert_eq!(tail, ["cica-worker:latest", "worker", "--turn", "turn-123"]);
    }

    #[tokio::test]
    async fn docker_container_guard_drop_while_armed_does_not_panic() {
        drop(DockerContainerGuard::new("cica-turn-test".into()));
    }

    #[test]
    fn docker_launcher_passes_env() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/c"),
            std::path::PathBuf::from("/s"),
            std::path::PathBuf::from("/st"),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);
        let args = l.run_args("t1");
        let e = args.iter().position(|a| a == "-e").unwrap();
        assert_eq!(args[e + 1], "CICA_FAKE_BACKEND=echo");
        let img = args.iter().position(|a| a == "cica-worker:latest").unwrap();
        assert!(e < img);
    }

    #[tokio::test]
    async fn run_worker_turn_reads_job_and_writes_result() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        push_job(&store, "tX", &sample_job()).await.unwrap();

        struct StubEngine;
        #[async_trait::async_trait]
        impl crate::sandbox::SandboxProvider for StubEngine {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                Ok(TurnResult {
                    response: "from-worker".into(),
                    backend_session_id: "sess-w".into(),
                    cost_usd: None,
                    duration_ms: None,
                })
            }
        }

        run_worker_turn(&store, &StubEngine, "tX").await.unwrap();
        let result = pull_result(&store, "tX").await.unwrap().unwrap();
        assert_eq!(result.response, "from-worker");
        assert_eq!(result.backend_session_id, "sess-w");
    }

    /// End-to-end Docker flow with the fake backend. Gated: only runs when
    /// `CICA_DOCKER_IT=1` (the CI docker-flow job, after building the image).
    /// Drives the real `cica-worker:latest` container + a tempdir filesystem
    /// store, asserting the turn round-trips with no real backend.
    #[tokio::test]
    async fn docker_flow_round_trips_with_fake_backend() {
        // Enabled by any non-empty `CICA_DOCKER_IT` value (the CI job sets `=1`).
        if std::env::var_os("CICA_DOCKER_IT").is_none() {
            return; // skipped in normal `cargo test`
        }

        use crate::config::AiBackend;

        let store_root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // Minimal config.toml to mount (backend is irrelevant — the fake hook
        // short-circuits before the real CLI call).
        let cfg_dir = tempfile::tempdir().unwrap();
        let config_file = cfg_dir.path().join("config.toml");
        std::fs::write(
            &config_file,
            "backend = \"cursor\"\n[deployment]\nstore = \"filesystem\"\n",
        )
        .unwrap();
        let skills_dir = tempfile::tempdir().unwrap();

        let launcher = DockerLauncher::new(
            "cica-worker:latest".into(),
            config_file,
            skills_dir.path().to_path_buf(),
            store_root.path().to_path_buf(),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);

        let provider = LaunchedWorkerProvider::new(store.clone(), Box::new(launcher));
        let job = TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "ping".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Cursor,
            model: None,
        };

        let result = provider.run_turn(job).await.expect("docker turn failed");
        assert!(
            result.response.contains("fake-response: ping"),
            "unexpected response: {}",
            result.response
        );
    }
}
