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

/// Store key for a turn's job blob.
fn job_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/job")
}

/// Store key for a turn's result blob.
fn result_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/result")
}

/// Store key for the whole turn subtree.
fn turn_prefix(turn_id: &str) -> String {
    format!("turns/{turn_id}")
}

/// A unique temp dir for staging a blob in/out of the store.
fn scratch_dir(turn_id: &str, kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cica-turn-{turn_id}-{kind}-{}", uuid::Uuid::new_v4()))
}

/// Serialize `job` into a fresh dir and push it under `turns/<id>/job`.
async fn push_job(store: &dyn StateStore, turn_id: &str, job: &TurnJob) -> Result<()> {
    let dir = scratch_dir(turn_id, "job");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("job.json"), serde_json::to_vec_pretty(job)?)?;
    store.push(&dir, &job_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Pull `turns/<id>/job` and deserialize the `TurnJob`.
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

/// Serialize `result` into a fresh dir and push it under `turns/<id>/result`.
async fn push_result(store: &dyn StateStore, turn_id: &str, result: &TurnResult) -> Result<()> {
    let dir = scratch_dir(turn_id, "result");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("result.json"), serde_json::to_vec_pretty(result)?)?;
    store.push(&dir, &result_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Pull `turns/<id>/result`; `None` if the worker never wrote one.
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

/// Worker side: pull the job, run it through `engine`, push the result.
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

/// Router-side provider: dispatches each turn to a `cica worker` child process.
pub struct SubprocessWorkerProvider {
    store: Arc<dyn StateStore>,
    self_exe: PathBuf,
}

impl SubprocessWorkerProvider {
    pub fn new(store: Arc<dyn StateStore>, self_exe: PathBuf) -> Self {
        Self { store, self_exe }
    }
}

#[async_trait]
impl SandboxProvider for SubprocessWorkerProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let turn_id = Uuid::new_v4().to_string();

        push_job(self.store.as_ref(), &turn_id, &job).await?;

        let status = Command::new(&self.self_exe)
            .arg("worker")
            .arg("--turn")
            .arg(&turn_id)
            .status()
            .await
            .context("spawning cica worker")?;

        if !status.success() {
            cleanup(self.store.as_ref(), &turn_id).await;
            anyhow::bail!("worker exited with status {status}");
        }

        let result = pull_result(self.store.as_ref(), &turn_id).await;
        cleanup(self.store.as_ref(), &turn_id).await;

        result?.ok_or_else(|| anyhow::anyhow!("worker produced no result for turn {turn_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiBackend;
    use crate::sandbox::state::FilesystemStateStore;

    fn sample_job() -> TurnJob {
        TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            cwd: None,
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
        assert_eq!(back.session_id, "telegram:1");
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
}
