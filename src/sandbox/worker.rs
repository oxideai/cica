//! Worker dispatch: run a turn in a one-shot `cica worker` child process,
//! exchanging the job and result through the `StateStore` keyed by a turn id.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::warn;
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

/// Attachments are keyed by path, so one upload serves every turn that names it.
fn attachment_key(path: &str) -> String {
    format!("attachments/{path}")
}

/// Files the agent produced during a turn, scoped to that turn: unlike inbound
/// attachments they are not worth sharing by name, and they are cleaned up with
/// the rest of the turn's blobs.
fn produced_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/out")
}

/// Beyond this, a "file the agent produced" is more likely a mistake than
/// something a colleague wants in a chat message.
const MAX_PRODUCED_BYTES: u64 = 100 * 1024 * 1024;

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

async fn push_attachments(store: &dyn StateStore, base: &std::path::Path, job: &TurnJob) {
    for relative in &job.attachments {
        let src = base.join(relative);
        if !src.is_file() {
            warn!("attachment {relative} not found at {src:?}; the worker will not see it");
            continue;
        }
        let Some(file_name) = src.file_name() else {
            warn!("attachment {relative} has no file name; the worker will not see it");
            continue;
        };
        let staging = std::env::temp_dir().join(format!("cica-attach-{}", Uuid::new_v4()));
        let copied = std::fs::create_dir_all(&staging)
            .and_then(|_| std::fs::copy(&src, staging.join(file_name)).map(|_| ()));
        if let Err(e) = copied {
            warn!("failed to stage attachment {relative}: {e}");
        } else if let Err(e) = store.push(&staging, &attachment_key(relative)).await {
            warn!("failed to push attachment {relative} to the store: {e}");
        }
        let _ = std::fs::remove_dir_all(&staging);
    }
}

/// Copy files the agent named with `[attachment:...]` into the store, so the
/// router can attach them to its reply.
///
/// A worker runs in a container that is gone by the time the router formats the
/// message, so a marker naming a worker-local path resolves to nothing and gets
/// printed to the user verbatim. That is the whole bug this exists to close.
///
/// Best-effort, like `push_attachments`: losing a file costs an attachment and
/// leaves the text, which is worth more than failing the turn. Names are
/// flattened to the file name so a marker cannot write outside the destination
/// directory on the far side.
async fn push_produced_files(store: &dyn StateStore, turn_id: &str, result: &mut TurnResult) {
    let markers = crate::sandbox::attachment_markers(&result.response);
    if markers.is_empty() {
        return;
    }

    let staging = std::env::temp_dir().join(format!("cica-produced-{}", Uuid::new_v4()));
    if let Err(e) = std::fs::create_dir_all(&staging) {
        warn!("failed to stage produced files: {e}");
        return;
    }

    let mut names = Vec::new();
    for marker in markers {
        let src = std::path::Path::new(&marker);
        let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if names.iter().any(|n| n == name) {
            warn!("two produced files share the name {name}; keeping the first");
            continue;
        }
        match std::fs::metadata(src) {
            Ok(meta) if !meta.is_file() => continue,
            Ok(meta) if meta.len() > MAX_PRODUCED_BYTES => {
                warn!(
                    "produced file {name} is {} bytes, over the {MAX_PRODUCED_BYTES} limit; not sending it",
                    meta.len()
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("produced file {marker} is not readable: {e}");
                continue;
            }
        }
        if let Err(e) = std::fs::copy(src, staging.join(name)) {
            warn!("failed to stage produced file {name}: {e}");
            continue;
        }
        names.push(name.to_string());
    }

    if !names.is_empty() {
        match store.push(&staging, &produced_key(turn_id)).await {
            Ok(()) => result.produced_files = names,
            Err(e) => warn!("failed to push produced files to the store: {e}"),
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
}

/// Bring the worker's produced files onto this machine and point the markers at
/// them, so the channel's existing `path.exists()` check passes.
///
/// Per-turn directory: file names come from the agent, and two turns naming the
/// same file must not overwrite each other mid-send.
async fn pull_produced_files(
    store: &dyn StateStore,
    turn_id: &str,
    dest_root: &std::path::Path,
    result: &mut TurnResult,
) {
    if result.produced_files.is_empty() {
        return;
    }
    let dest = dest_root.join(turn_id);
    if let Err(e) = std::fs::create_dir_all(&dest) {
        warn!("failed to create {dest:?} for produced files: {e}");
        return;
    }
    match store.pull(&produced_key(turn_id), &dest).await {
        Ok(true) => result.response = rewrite_markers(&result.response, &dest),
        Ok(false) => warn!("worker reported produced files but the store had none"),
        Err(e) => warn!("failed to pull produced files: {e}"),
    }
}

/// Repoint each `[attachment:...]` marker at `dir/<file name>` when that file
/// landed. A marker whose file is missing is left alone: the channel drops it
/// for not existing, which is the pre-existing behaviour.
fn rewrite_markers(response: &str, dir: &std::path::Path) -> String {
    let mut out = response.to_string();
    for marker in crate::sandbox::attachment_markers(response) {
        let Some(name) = std::path::Path::new(&marker).file_name() else {
            continue;
        };
        let local = dir.join(name);
        if local.is_file() {
            out = out.replace(
                &format!("[attachment:{marker}]"),
                &format!("[attachment:{}]", local.display()),
            );
        }
    }
    out
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
async fn cleanup(store: &dyn StateStore, turn_id: &str) {
    let _ = store.delete(&turn_prefix(turn_id)).await;
}

pub async fn run_worker_turn(
    store: &dyn StateStore,
    engine: &dyn crate::sandbox::SandboxProvider,
    turn_id: &str,
) -> Result<()> {
    let job = pull_job(store, turn_id).await?;
    let mut result = engine.run_turn(job).await?;
    push_produced_files(store, turn_id, &mut result).await;
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
    base: PathBuf,
}

impl LaunchedWorkerProvider {
    pub fn new(store: Arc<dyn StateStore>, launcher: Box<dyn Launcher>, base: PathBuf) -> Self {
        Self {
            store,
            launcher,
            base,
        }
    }

    /// Where produced files land on this machine: alongside each channel's
    /// inbound attachment directory under `internal/`, but its own, so an
    /// outbound file cannot overwrite an inbound one of the same name.
    ///
    /// Derived from `base` rather than taking a path of its own, because #56
    /// made attachment paths relative to the workspace root and left this
    /// provider holding only `base`.
    fn produced_dir(&self) -> PathBuf {
        self.base.join("internal").join("worker_outputs")
    }
}

#[async_trait]
impl SandboxProvider for LaunchedWorkerProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let turn_id = Uuid::new_v4().to_string();

        push_attachments(self.store.as_ref(), &self.base, &job).await;
        push_job(self.store.as_ref(), &turn_id, &job).await?;

        if let Err(e) = self.launcher.launch(&turn_id).await {
            cleanup(self.store.as_ref(), &turn_id).await;
            return Err(e);
        }

        let result = pull_result(self.store.as_ref(), &turn_id).await;

        // Before cleanup: it deletes the whole turn prefix, produced files included.
        let result = match result {
            Ok(Some(mut result)) => {
                pull_produced_files(
                    self.store.as_ref(),
                    &turn_id,
                    &self.produced_dir(),
                    &mut result,
                )
                .await;
                Ok(Some(result))
            }
            other => other,
        };
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
    skills_dir: Option<PathBuf>,
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
    /// `skills_dir` is `None` when the state store carries the skills.
    pub fn new(
        image: String,
        config_file: PathBuf,
        skills_dir: Option<PathBuf>,
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
        if let Some(skills_dir) = &self.skills_dir {
            args.push("-v".into());
            args.push(format!("{}:/data/cica/skills:ro", skills_dir.display()));
        }
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
            attachments: Vec::new(),
        }
    }

    fn sample_result(response: &str) -> TurnResult {
        TurnResult {
            response: response.into(),
            backend_session_id: "s1".into(),
            cost_usd: None,
            duration_ms: None,
            produced_files: Vec::new(),
        }
    }

    /// The bug: a worker writes a file inside its container and names it with a
    /// marker. The router has no such path, the channel's existence check fails,
    /// and the marker is printed to the user as text.
    #[tokio::test]
    async fn a_file_the_worker_wrote_reaches_the_router() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());

        // Worker side: a file only this machine can see.
        let worker_dir = tempfile::tempdir().unwrap();
        let produced = worker_dir.path().join("cft_input.json");
        std::fs::write(&produced, br#"{"herd": 120}"#).unwrap();
        let mut result = sample_result(&format!(
            "[attachment:{}]\n\nHere is the CFT input.",
            produced.display()
        ));

        push_produced_files(&store, "t-out", &mut result).await;
        assert_eq!(result.produced_files, vec!["cft_input.json".to_string()]);

        // Router side: a different directory, and the worker's path is gone.
        std::fs::remove_dir_all(worker_dir.path()).unwrap();
        let router_root = tempfile::tempdir().unwrap();
        pull_produced_files(&store, "t-out", router_root.path(), &mut result).await;

        let landed = router_root.path().join("t-out").join("cft_input.json");
        assert!(landed.is_file(), "the file did not reach the router");
        assert_eq!(std::fs::read(&landed).unwrap(), br#"{"herd": 120}"#);
        assert!(
            result
                .response
                .contains(&format!("[attachment:{}]", landed.display())),
            "the marker still points at the worker path: {}",
            result.response
        );
    }

    #[tokio::test]
    async fn a_turn_with_no_marker_ships_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let mut result = sample_result("Just an answer, no files.");
        push_produced_files(&store, "t-none", &mut result).await;
        assert!(result.produced_files.is_empty());
    }

    /// A marker naming a path that is not there must not break the turn: the
    /// text still goes out, just without an attachment.
    #[tokio::test]
    async fn a_marker_for_a_missing_file_is_survivable() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let mut result = sample_result("[attachment:/nope/gone.json]\n\nText survives.");
        push_produced_files(&store, "t-missing", &mut result).await;
        assert!(result.produced_files.is_empty());
        assert!(result.response.contains("Text survives."));
    }

    /// Names come from the agent, so a marker must not be able to write outside
    /// the destination directory on the router.
    #[tokio::test]
    async fn a_traversing_name_is_flattened() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let evil = dir.path().join("passwd");
        std::fs::write(&evil, b"x").unwrap();

        let mut result = sample_result(&format!("[attachment:{}]", evil.display()));
        push_produced_files(&store, "t-evil", &mut result).await;
        assert_eq!(result.produced_files, vec!["passwd".to_string()]);
        assert!(
            !result.produced_files.iter().any(|n| n.contains('/')),
            "a stored name kept a path separator"
        );
    }

    /// An older worker's result has no `produced_files` field at all.
    #[test]
    fn a_result_without_produced_files_still_deserializes() {
        let json =
            r#"{"response":"hi","backend_session_id":"s","cost_usd":null,"duration_ms":null}"#;
        let r: TurnResult = serde_json::from_str(json).unwrap();
        assert!(r.produced_files.is_empty());
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
            produced_files: Vec::new(),
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
                    produced_files: Vec::new(),
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
            root.path().join("base"),
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
            attachments: Vec::new(),
        };
        let result = provider.run_turn(job).await.unwrap();
        assert_eq!(result.backend_session_id, "sess");
    }

    #[test]
    fn docker_launcher_builds_run_args_with_skills_mount() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/host/config.toml"),
            Some(std::path::PathBuf::from("/host/skills")),
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

    #[test]
    fn docker_launcher_builds_run_args_without_skills_mount() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/host/config.toml"),
            None,
            std::path::PathBuf::from("/host/state-store"),
        );
        let args = l.run_args("turn-123");
        assert!(!args.iter().any(|arg| arg.contains(":/data/cica/skills")));
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
            Some(std::path::PathBuf::from("/s")),
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
                    produced_files: Vec::new(),
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
            Some(skills_dir.path().to_path_buf()),
            store_root.path().to_path_buf(),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);

        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(launcher),
            cfg_dir.path().join("base"),
        );
        let job = TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "ping".into(),
            system_prompt: None,
            resume_session: None,
            skip_permissions: true,
            backend: AiBackend::Cursor,
            model: None,
            attachments: Vec::new(),
        };

        let result = provider.run_turn(job).await.expect("docker turn failed");
        assert!(
            result.response.contains("fake-response: ping"),
            "unexpected response: {}",
            result.response
        );
    }
}
