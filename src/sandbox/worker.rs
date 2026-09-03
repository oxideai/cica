//! Worker dispatch: run a turn in a one-shot `cica worker` child process,
//! exchanging the job and result through the `StateStore` keyed by a turn id.

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
#[cfg(test)]
use tokio::time::{Instant, sleep};
use tracing::warn;
use uuid::Uuid;

use crate::sandbox::state::StateStore;
use crate::sandbox::{
    PROTOCOL_VERSION, SandboxProvider, TurnEnvelope, TurnJob, TurnOutcome, TurnResult,
};

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

/// Per turn, unlike inbound attachments: the agent picks these names, so two
/// turns can pick the same one.
fn produced_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/out")
}

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

async fn push_result(store: &dyn StateStore, envelope: &TurnEnvelope) -> Result<()> {
    store
        .put_record(
            &result_key(&envelope.turn_id),
            &serde_json::to_vec(envelope)?,
        )
        .await
}

/// `None` if the worker never wrote a result.
async fn pull_result(store: &dyn StateStore, turn_id: &str) -> Result<Option<TurnEnvelope>> {
    store
        .get_record(&result_key(turn_id))
        .await?
        .map(|bytes| serde_json::from_slice(&bytes).context("deserializing TurnEnvelope"))
        .transpose()
}

/// Best-effort removal of a turn's blobs after the router has the result.
async fn cleanup(store: &dyn StateStore, turn_id: &str) {
    let _ = store.delete_record(&result_key(turn_id)).await;
    let _ = store.delete(&turn_prefix(turn_id)).await;
}

pub async fn run_worker_turn(
    store: &dyn StateStore,
    engine: &dyn crate::sandbox::SandboxProvider,
    turn_id: &str,
) -> Result<()> {
    static WORKER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let job = pull_job(store, turn_id).await?;
    let affinity_id = job.affinity.id();
    let outcome = match engine.run_turn(job).await {
        Ok(mut result) => {
            push_produced_files(store, turn_id, &mut result).await;
            TurnOutcome::Result(result)
        }
        Err(error) => TurnOutcome::Error(error.to_string()),
    };
    push_result(
        store,
        &TurnEnvelope {
            protocol_version: PROTOCOL_VERSION,
            affinity_id,
            turn_id: turn_id.to_string(),
            worker_id: WORKER_ID.get_or_init(|| Uuid::new_v4().to_string()).clone(),
            outcome,
        },
    )
    .await?;
    Ok(())
}

/// Configuration passed to a persistent worker process.
#[derive(Debug, Clone)]
#[cfg(test)]
pub struct WorkerSpec {
    pub session: String,
    pub worker_id: String,
    pub launch_token: String,
    pub idle: Duration,
    pub turn_timeout: Duration,
    pub start_timeout: Duration,
}

#[cfg(test)]
impl WorkerSpec {
    /// Returns the stable worker command-line contract.
    pub fn args(&self) -> Vec<String> {
        vec![
            "worker".into(),
            "--session".into(),
            self.session.clone(),
            "--worker-id".into(),
            self.worker_id.clone(),
            "--idle-secs".into(),
            self.idle.as_secs().to_string(),
            "--turn-timeout-secs".into(),
            self.turn_timeout.as_secs().to_string(),
        ]
    }
}

/// Platform used to create a worker handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg(test)]
pub enum LauncherKind {
    Subprocess,
    Docker,
    Fargate,
}

/// Serializable identity of a worker on its launch platform.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg(test)]
pub struct Handle {
    pub kind: LauncherKind,
    pub id: String,
}

/// Observed worker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum Status {
    Running,
    Stopped,
    NotFound,
    Unknown,
}

/// Result of waiting for a worker to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum StopOutcome {
    Terminated,
    NotFound,
    Unknown,
}

/// Creates, observes, and stops workers while retaining one-shot dispatch.
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Starts a worker and returns only after the platform reports it running.
    #[cfg(test)]
    async fn start(&self, _spec: &WorkerSpec) -> Result<Handle> {
        anyhow::bail!("persistent workers are not supported by this launcher")
    }
    /// Reads the current platform state for a worker handle.
    #[cfg(test)]
    async fn status(&self, _handle: &Handle) -> Result<Status> {
        Ok(Status::Unknown)
    }
    /// Requests termination and waits no longer than `deadline`.
    #[cfg(test)]
    async fn stop_and_wait(&self, _handle: &Handle, _deadline: Duration) -> Result<StopOutcome> {
        Ok(StopOutcome::Unknown)
    }
    /// Finds a worker previously started with the spec's launch token.
    #[cfg(test)]
    async fn reconcile(&self, _spec: &WorkerSpec) -> Result<Option<Handle>> {
        Ok(None)
    }
    /// Runs a one-shot worker turn to completion.
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

    /// Kept apart from inbound attachments so an outbound file cannot overwrite one of the same name.
    fn produced_dir(&self) -> PathBuf {
        self.base.join("internal/attachments/outbound")
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

        let expected_affinity = job.affinity.id();
        let result = pull_result(self.store.as_ref(), &turn_id).await;

        // cleanup deletes the whole turn prefix, produced files included.
        let result = match result {
            Ok(Some(envelope)) => {
                if envelope.turn_id != turn_id || envelope.affinity_id != expected_affinity {
                    cleanup(self.store.as_ref(), &turn_id).await;
                    anyhow::bail!(
                        "worker result identity mismatch: turn_id expected {turn_id}, got {}; affinity_id expected {expected_affinity}, got {}",
                        envelope.turn_id,
                        envelope.affinity_id
                    );
                }
                if envelope.protocol_version != PROTOCOL_VERSION {
                    cleanup(self.store.as_ref(), &turn_id).await;
                    anyhow::bail!(
                        "unsupported worker protocol version {}",
                        envelope.protocol_version
                    );
                }
                let mut result = match envelope.outcome {
                    TurnOutcome::Result(result) => result,
                    TurnOutcome::Error(error) => {
                        cleanup(self.store.as_ref(), &turn_id).await;
                        return Err(anyhow::anyhow!(error));
                    }
                };
                pull_produced_files(
                    self.store.as_ref(),
                    &turn_id,
                    &self.produced_dir(),
                    &mut result,
                )
                .await;
                Ok(Some(result))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        };
        cleanup(self.store.as_ref(), &turn_id).await;

        result?.ok_or_else(|| anyhow::anyhow!("worker produced no result for turn {turn_id}"))
    }
}

/// Launcher that spawns `cica worker --turn <id>` as a local child process.
pub struct SubprocessLauncher {
    self_exe: PathBuf,
    router_paths: crate::config::Paths,
}

impl SubprocessLauncher {
    pub fn new(self_exe: PathBuf, router_paths: crate::config::Paths) -> Self {
        Self {
            self_exe,
            router_paths,
        }
    }

    fn worker_home(&self, worker_id: &str) -> PathBuf {
        self.router_paths
            .internal_dir
            .join("workers")
            .join(worker_id)
    }

    fn isolation_args(&self, home: &Path) -> Vec<String> {
        vec![
            "--home".into(),
            home.display().to_string(),
            "--deps".into(),
            self.router_paths.deps_dir.display().to_string(),
            "--skills".into(),
            self.router_paths.skills_dir.display().to_string(),
            "--config".into(),
            self.router_paths.config_file.display().to_string(),
        ]
    }
}

#[cfg(test)]
async fn process_start_time(pid: u32) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return Ok(None);
        }
        let info = unsafe { info.assume_init() };
        Ok(Some(format!(
            "{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        )))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let fields = stat
            .rsplit_once(')')
            .context("invalid /proc pid stat")?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        Ok(fields.get(19).map(|value| (*value).to_string()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .await
            .context("reading process start time")?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
}

#[cfg(test)]
fn read_pid_file(path: &Path) -> Result<(u32, String)> {
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("reading subprocess handle {}", path.display()))?;
    let (pid, start_time) = value
        .trim()
        .split_once(':')
        .context("invalid subprocess handle")?;
    Ok((
        pid.parse().context("invalid subprocess pid")?,
        start_time.into(),
    ))
}

#[cfg(test)]
fn signal_process(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
async fn subprocess_matches(path: &Path) -> Result<Option<u32>> {
    let (pid, expected) = match read_pid_file(path) {
        Ok(value) => value,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if signal_process(pid, 0).is_err() {
        return Ok(None);
    }
    Ok((process_start_time(pid).await?.as_deref() == Some(expected.as_str())).then_some(pid))
}

#[async_trait]
impl Launcher for SubprocessLauncher {
    #[cfg(test)]
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
        let home = self.worker_home(&spec.worker_id);
        std::fs::create_dir_all(&home)?;
        let mut args = spec.args();
        args.extend(self.isolation_args(&home));
        let mut child = Command::new(&self.self_exe)
            .args(args)
            .kill_on_drop(false)
            .spawn()
            .context("spawning cica worker")?;
        let pid = child.id().context("spawned worker has no pid")?;
        let start_time = process_start_time(pid)
            .await?
            .context("worker exited before its start time could be read")?;
        let pid_file = home.join(format!("launch.{}.pid", spec.launch_token));
        std::fs::write(&pid_file, format!("{pid}:{start_time}"))?;
        sleep(Duration::from_secs(2)).await;
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("worker exited during startup with status {status}");
        }
        Ok(Handle {
            kind: LauncherKind::Subprocess,
            id: pid_file.display().to_string(),
        })
    }

    #[cfg(test)]
    async fn status(&self, handle: &Handle) -> Result<Status> {
        Ok(
            if subprocess_matches(Path::new(&handle.id)).await?.is_some() {
                Status::Running
            } else {
                Status::NotFound
            },
        )
    }

    #[cfg(test)]
    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome> {
        let path = Path::new(&handle.id);
        let Some(pid) = subprocess_matches(path).await? else {
            return Ok(StopOutcome::NotFound);
        };
        if signal_process(pid, libc::SIGTERM).is_err() {
            return Ok(StopOutcome::NotFound);
        }
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if subprocess_matches(path).await?.is_none() {
                return Ok(StopOutcome::Terminated);
            }
            sleep(Duration::from_millis(50)).await;
        }
        let _ = signal_process(pid, libc::SIGKILL);
        sleep(Duration::from_millis(50)).await;
        Ok(if subprocess_matches(path).await?.is_none() {
            StopOutcome::Terminated
        } else {
            StopOutcome::Unknown
        })
    }

    #[cfg(test)]
    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
        let path = self
            .worker_home(&spec.worker_id)
            .join(format!("launch.{}.pid", spec.launch_token));
        Ok(subprocess_matches(&path).await?.map(|_| Handle {
            kind: LauncherKind::Subprocess,
            id: path.display().to_string(),
        }))
    }

    async fn launch(&self, turn_id: &str) -> Result<()> {
        let home = self.worker_home(&Uuid::new_v4().to_string());
        std::fs::create_dir_all(&home)?;
        let status = Command::new(&self.self_exe)
            .arg("worker")
            .arg("--turn")
            .arg(turn_id)
            .args(self.isolation_args(&home))
            .kill_on_drop(true)
            .status()
            .await
            .context("spawning cica worker");
        let _ = std::fs::remove_dir_all(&home);
        let status = status?;
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
            "-e".into(),
            "CICA_STATE_PATH=/data/cica/internal/state-store".into(),
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

    #[cfg(test)]
    fn worker_name(spec: &WorkerSpec) -> String {
        let slug = spec.session.chars().take(24).collect::<String>();
        format!("cica-{slug}-{}", spec.launch_token)
    }

    #[cfg(test)]
    fn worker_run_args(&self, spec: &WorkerSpec) -> Vec<String> {
        let name = Self::worker_name(spec);
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--name".into(),
            name,
            "--label".into(),
            format!("cica.session={}", spec.session),
            "--label".into(),
            format!("cica.launch_token={}", spec.launch_token),
            "-e".into(),
            "CICA_STATE_PATH=/data/cica/internal/state-store".into(),
        ];
        for (key, value) in &self.env {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.extend([
            "-v".into(),
            format!("{}:/data/cica/config.toml:ro", self.config_file.display()),
        ]);
        if let Some(skills_dir) = &self.skills_dir {
            args.extend([
                "-v".into(),
                format!("{}:/data/cica/skills:ro", skills_dir.display()),
            ]);
        }
        args.extend([
            "-v".into(),
            format!(
                "{}:/data/cica/internal/state-store",
                self.state_store_dir.display()
            ),
            self.image.clone(),
        ]);
        args.extend(spec.args());
        args
    }

    #[cfg(test)]
    async fn inspect_running(name: &str) -> Result<Option<bool>> {
        let output = Command::new("docker")
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .await
            .context("running docker inspect")?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim() == "true",
        ))
    }
}

#[async_trait]
impl Launcher for DockerLauncher {
    #[cfg(test)]
    async fn start(&self, spec: &WorkerSpec) -> Result<Handle> {
        let name = Self::worker_name(spec);
        let output = Command::new("docker")
            .args(self.worker_run_args(spec))
            .output()
            .await
            .context("starting cica worker container")?;
        if !output.status.success() {
            anyhow::bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        sleep(Duration::from_secs(2)).await;
        if Self::inspect_running(&name).await? != Some(true) {
            anyhow::bail!("worker container {name} exited during startup");
        }
        Ok(Handle {
            kind: LauncherKind::Docker,
            id: name,
        })
    }

    #[cfg(test)]
    async fn status(&self, handle: &Handle) -> Result<Status> {
        Ok(match Self::inspect_running(&handle.id).await? {
            Some(true) => Status::Running,
            Some(false) => Status::Stopped,
            None => Status::NotFound,
        })
    }

    #[cfg(test)]
    async fn stop_and_wait(&self, handle: &Handle, deadline: Duration) -> Result<StopOutcome> {
        if Self::inspect_running(&handle.id).await?.is_none() {
            return Ok(StopOutcome::NotFound);
        }
        let status = Command::new("docker")
            .args(["stop", "-t", &deadline.as_secs().to_string(), &handle.id])
            .status()
            .await
            .context("stopping cica worker container")?;
        if !status.success() && Self::inspect_running(&handle.id).await?.is_some() {
            return Ok(StopOutcome::Unknown);
        }
        Ok(match Self::inspect_running(&handle.id).await? {
            None | Some(false) => StopOutcome::Terminated,
            Some(true) => StopOutcome::Unknown,
        })
    }

    #[cfg(test)]
    async fn reconcile(&self, spec: &WorkerSpec) -> Result<Option<Handle>> {
        let output = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=cica.launch_token={}", spec.launch_token),
            ])
            .output()
            .await
            .context("reconciling cica worker container")?;
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            return Ok(None);
        }
        let name = Self::worker_name(spec);
        Ok(Some(Handle {
            kind: LauncherKind::Docker,
            id: name,
        }))
    }

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
    use crate::config::{AiBackend, Paths};
    use crate::sandbox::state::FilesystemStateStore;

    fn sample_job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "1".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
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

    fn worker_spec() -> WorkerSpec {
        WorkerSpec {
            session: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            worker_id: "worker-1".into(),
            launch_token: "token-1".into(),
            idle: Duration::from_secs(600),
            turn_timeout: Duration::from_secs(900),
            start_timeout: Duration::from_secs(180),
        }
    }

    #[test]
    fn worker_spec_builds_worker_args() {
        assert_eq!(worker_spec().start_timeout, Duration::from_secs(180));
        assert_eq!(
            worker_spec().args(),
            [
                "worker",
                "--session",
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
                "--worker-id",
                "worker-1",
                "--idle-secs",
                "600",
                "--turn-timeout-secs",
                "900"
            ]
        );
    }

    #[tokio::test]
    async fn subprocess_worker_starts_reconciles_and_stops() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("worker.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::for_base(root.path().join("router"));
        let launcher = SubprocessLauncher::new(script, paths);
        let spec = worker_spec();

        let handle = launcher.start(&spec).await.unwrap();
        assert_eq!(launcher.status(&handle).await.unwrap(), Status::Running);
        assert_eq!(
            launcher.reconcile(&spec).await.unwrap(),
            Some(handle.clone())
        );
        assert_eq!(
            launcher
                .stop_and_wait(&handle, Duration::from_secs(1))
                .await
                .unwrap(),
            StopOutcome::Terminated
        );
        assert_eq!(launcher.status(&handle).await.unwrap(), Status::NotFound);
    }

    #[tokio::test]
    async fn subprocess_reconcile_rejects_changed_start_time() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("worker.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::for_base(root.path().join("router"));
        let launcher = SubprocessLauncher::new(script, paths);
        let spec = worker_spec();
        let handle = launcher.start(&spec).await.unwrap();
        let (pid, _) = read_pid_file(Path::new(&handle.id)).unwrap();
        std::fs::write(&handle.id, format!("{pid}:different start time")).unwrap();

        assert!(launcher.reconcile(&spec).await.unwrap().is_none());
        let _ = signal_process(pid, libc::SIGKILL);
    }

    #[tokio::test]
    async fn subprocess_turns_use_distinct_worker_homes() {
        use std::os::unix::fs::PermissionsExt;

        let Some(binary) = std::env::var_os("CICA_BIN") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let router = Paths::for_base(root.path().join("router"));
        std::fs::create_dir_all(&router.base).unwrap();
        let state = router.internal_dir.join("state-store");
        std::fs::write(
            &router.config_file,
            format!(
                "backend = \"cursor\"\n[deployment]\nstore = \"filesystem\"\nstate_path = {:?}\n",
                state.display().to_string()
            ),
        )
        .unwrap();
        let wrapper = root.path().join("cica-test-worker");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nCICA_FAKE_BACKEND=1 exec {:?} \"$@\"\n",
                PathBuf::from(binary).display().to_string()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Arc::new(FilesystemStateStore::new(state));
        push_job(store.as_ref(), "turn-a", &sample_job())
            .await
            .unwrap();
        push_job(store.as_ref(), "turn-b", &sample_job())
            .await
            .unwrap();
        let launcher = Arc::new(SubprocessLauncher::new(wrapper, router.clone()));
        let first = {
            let launcher = launcher.clone();
            tokio::spawn(async move { launcher.launch("turn-a").await })
        };
        let second = {
            let launcher = launcher.clone();
            tokio::spawn(async move { launcher.launch("turn-b").await })
        };
        let workers = router.internal_dir.join("workers");
        let mut distinct = false;
        while !first.is_finished() || !second.is_finished() {
            distinct = std::fs::read_dir(&workers)
                .map(|entries| entries.filter_map(|entry| entry.ok()).count() >= 2)
                .unwrap_or(false);
            if distinct {
                break;
            }
            tokio::task::yield_now().await;
        }
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert!(distinct);
        assert!(
            pull_result(store.as_ref(), "turn-a")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            pull_result(store.as_ref(), "turn-b")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn docker_worker_run_args_carry_identity_and_contract() {
        let launcher = DockerLauncher::new(
            "image".into(),
            PathBuf::from("/config"),
            Some(PathBuf::from("/skills")),
            PathBuf::from("/state"),
        );
        let args = launcher.worker_run_args(&worker_spec());

        assert!(args.contains(&"cica.session=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into()));
        assert!(args.contains(&"cica.launch_token=token-1".into()));
        assert!(args.contains(&"cica-abcdefghijklmnopqrstuvwx-token-1".into()));
        assert_eq!(&args[args.len() - 9..], worker_spec().args());
    }

    #[tokio::test]
    async fn a_file_the_worker_wrote_reaches_the_router() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());

        let worker_dir = tempfile::tempdir().unwrap();
        let produced = worker_dir.path().join("cft_input.json");
        std::fs::write(&produced, br#"{"herd": 120}"#).unwrap();
        let mut result = sample_result(&format!(
            "[attachment:{}]\n\nHere is the CFT input.",
            produced.display()
        ));

        push_produced_files(&store, "t-out", &mut result).await;
        assert_eq!(result.produced_files, vec!["cft_input.json".to_string()]);

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

    #[tokio::test]
    async fn a_marker_for_a_missing_file_is_survivable() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let mut result = sample_result("[attachment:/nope/gone.json]\n\nText survives.");
        push_produced_files(&store, "t-missing", &mut result).await;
        assert!(result.produced_files.is_empty());
        assert!(result.response.contains("Text survives."));
    }

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
        push_result(
            &store,
            &TurnEnvelope {
                protocol_version: PROTOCOL_VERSION,
                affinity_id: sample_job().affinity.id(),
                turn_id: "t2".into(),
                worker_id: "w1".into(),
                outcome: TurnOutcome::Result(result),
            },
        )
        .await
        .unwrap();
        let back = pull_result(&store, "t2").await.unwrap().unwrap();
        let TurnOutcome::Result(back) = back.outcome else {
            panic!("expected result")
        };
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
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
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

    #[tokio::test]
    async fn launched_provider_maps_error_envelope_to_error() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().to_path_buf()));
        struct FailingEngine;
        #[async_trait]
        impl SandboxProvider for FailingEngine {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                anyhow::bail!("backend failed")
            }
        }
        struct Fake {
            store: Arc<FilesystemStateStore>,
        }
        #[async_trait]
        impl Launcher for Fake {
            async fn launch(&self, turn_id: &str) -> Result<()> {
                run_worker_turn(self.store.as_ref(), &FailingEngine, turn_id).await
            }
        }
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(Fake { store }),
            root.path().join("base"),
        );
        assert!(
            provider
                .run_turn(sample_job())
                .await
                .unwrap_err()
                .to_string()
                .contains("backend failed")
        );
    }

    #[tokio::test]
    async fn launched_provider_rejects_result_identity_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(root.path().to_path_buf()));
        struct Fake {
            store: Arc<FilesystemStateStore>,
        }
        #[async_trait]
        impl Launcher for Fake {
            async fn launch(&self, turn_id: &str) -> Result<()> {
                let envelope = TurnEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    affinity_id: "wrong-affinity".into(),
                    turn_id: format!("wrong-{turn_id}"),
                    worker_id: "w1".into(),
                    outcome: TurnOutcome::Result(sample_result("ok")),
                };
                self.store
                    .put_record(&result_key(turn_id), &serde_json::to_vec(&envelope)?)
                    .await
            }
        }
        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(Fake { store }),
            root.path().join("base"),
        );
        let error = provider
            .run_turn(sample_job())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("turn_id expected") && error.contains("affinity_id expected"));
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
        assert!(args.contains(&"CICA_STATE_PATH=/data/cica/internal/state-store".to_string()));
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
        let e = args
            .iter()
            .position(|a| a == "CICA_FAKE_BACKEND=echo")
            .unwrap();
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
        let TurnOutcome::Result(result) = result.outcome else {
            panic!("expected result")
        };
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
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
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
