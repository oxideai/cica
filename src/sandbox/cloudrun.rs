//! Cloud Run launcher (feature `cloudrun`).
//!
//! Implements `Launcher` via Cloud Run `RunJob` + a `GetExecution` poll, reusing
//! the store-mediated job/result protocol. Google Cloud calls sit behind the
//! `CloudRunClient` trait so the launch/poll/cancel state machine is testable
//! without GCP; `google-cloud-run-v2` lives only in `GcpRunClient` (Task 7).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::OnceCell;
use tokio::time::{Duration, Instant, sleep};

use crate::config::CloudRunConfig;
use crate::sandbox::worker::Launcher;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunJobRequest {
    pub project: String,
    pub region: String,
    pub job: String,
    pub container_name: Option<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionStatus {
    pub terminal: bool,
    pub succeeded: bool,
    pub reason: Option<String>,
}

/// What CloudRunLauncher needs from Cloud Run — keeps google-cloud-run-v2 at the edge.
#[async_trait]
pub(crate) trait CloudRunClient: Send + Sync {
    async fn run_job(&self, req: &RunJobRequest) -> Result<String>;
    async fn get_execution(&self, execution: &str) -> Result<ExecutionStatus>;
    async fn cancel_execution(&self, execution: &str) -> Result<()>;
}

pub struct CloudRunLauncher {
    run: Box<dyn CloudRunClient>,
    config: CloudRunConfig,
}

impl CloudRunLauncher {
    /// Build the GCP-backed launcher (lazy client). Real client is Task 7.
    pub fn new(config: CloudRunConfig) -> Self {
        Self::with_client(Box::new(GcpRunClient::new(&config)), config)
    }

    pub(crate) fn with_client(run: Box<dyn CloudRunClient>, config: CloudRunConfig) -> Self {
        Self { run, config }
    }

    pub(crate) fn run_job_request(&self, turn_id: &str) -> RunJobRequest {
        RunJobRequest {
            project: self.config.project.clone(),
            region: self.config.region.clone(),
            job: self.config.job.clone(),
            container_name: self.config.container_name.clone(),
            args: vec!["worker".into(), "--turn".into(), turn_id.into()],
        }
    }
}

#[async_trait]
impl Launcher for CloudRunLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let exec = self.run.run_job(&self.run_job_request(turn_id)).await?;
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_secs);
        let interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            let st = self.run.get_execution(&exec).await?;
            if st.terminal {
                return if st.succeeded {
                    Ok(())
                } else {
                    bail!("worker execution failed (reason {:?})", st.reason)
                };
            }
            if Instant::now() >= deadline {
                if let Err(e) = self.run.cancel_execution(&exec).await {
                    tracing::warn!("failed to cancel timed-out execution {exec}: {e}");
                }
                bail!(
                    "worker execution timed out after {}s",
                    self.config.timeout_secs
                );
            }
            sleep(interval).await;
        }
    }
}

// --- Real google-cloud-run-v2 client (ADC), mirroring AwsEcsClient's lazy pattern. ---

/// The two Cloud Run clients we need, built once on first use.
struct RunClients {
    jobs: google_cloud_run_v2::client::Jobs,
    executions: google_cloud_run_v2::client::Executions,
}

/// Real `CloudRunClient` over `google-cloud-run-v2`, with lazily-built ADC clients.
///
/// Holds no config: every call gets its target (project/region/job) from the
/// `RunJobRequest` the launcher builds, and ADC resolves credentials itself.
pub(crate) struct GcpRunClient {
    clients: OnceCell<RunClients>,
}

/// The fully-qualified Cloud Run Job resource name. Pure, for testing.
fn job_name(project: &str, region: &str, job: &str) -> String {
    format!("projects/{project}/locations/{region}/jobs/{job}")
}

impl GcpRunClient {
    pub(crate) fn new(_config: &CloudRunConfig) -> Self {
        Self {
            clients: OnceCell::new(),
        }
    }

    async fn clients(&self) -> Result<&RunClients> {
        self.clients
            .get_or_try_init(|| async {
                // ADC by default (resolved on the worker / Cloud Run service account).
                let jobs = google_cloud_run_v2::client::Jobs::builder()
                    .build()
                    .await
                    .context("building Cloud Run Jobs client")?;
                let executions = google_cloud_run_v2::client::Executions::builder()
                    .build()
                    .await
                    .context("building Cloud Run Executions client")?;
                Ok::<_, anyhow::Error>(RunClients { jobs, executions })
            })
            .await
    }
}

#[async_trait]
impl CloudRunClient for GcpRunClient {
    async fn run_job(&self, req: &RunJobRequest) -> Result<String> {
        use google_cloud_lro::{Poller, PollingResult};
        use google_cloud_run_v2::model::run_job_request::{
            Overrides, overrides::ContainerOverride,
        };

        let mut container = ContainerOverride::new().set_args(req.args.clone());
        // Name targets a specific container; omitted (empty) for a sole-container job.
        if let Some(name) = &req.container_name {
            container = container.set_name(name.clone());
        }
        let overrides = Overrides::new().set_container_overrides([container]);
        let name = job_name(&req.project, &req.region, &req.job);

        // Fire RunJob and read the execution resource name from the first poll —
        // do NOT await the job to completion (we poll GetExecution on our cadence).
        let mut poller = self
            .clients()
            .await?
            .jobs
            .run_job()
            .set_name(name)
            .set_overrides(overrides)
            .poller();
        match poller.poll().await {
            Some(PollingResult::InProgress(Some(exec))) => Ok(exec.name),
            Some(PollingResult::Completed(res)) => {
                Ok(res.context("run_job execution completed with error")?.name)
            }
            Some(PollingResult::InProgress(None)) => {
                bail!("run_job: no execution metadata on first poll")
            }
            Some(PollingResult::PollingError(e)) => {
                Err(anyhow::Error::from(e).context("run_job: polling error"))
            }
            None => bail!("run_job: poller returned no result"),
        }
    }

    async fn get_execution(&self, execution: &str) -> Result<ExecutionStatus> {
        use google_cloud_run_v2::model::condition::State;

        let exec = self
            .clients()
            .await?
            .executions
            .get_execution()
            .set_name(execution)
            .send()
            .await
            .context("cloud run get_execution")?;

        let terminal = exec.completion_time.is_some();
        // Count-based success decision (avoids depending on a success-state enum variant).
        let succeeded =
            exec.succeeded_count > 0 && exec.failed_count == 0 && exec.cancelled_count == 0;
        let reason = if terminal && !succeeded {
            exec.conditions
                .iter()
                .find(|c| c.state == State::ConditionFailed)
                .map(|c| c.message.clone())
        } else {
            None
        };

        Ok(ExecutionStatus {
            terminal,
            succeeded,
            reason,
        })
    }

    async fn cancel_execution(&self, execution: &str) -> Result<()> {
        // Best-effort, also an LRO: fire send() and don't await completion.
        self.clients()
            .await?
            .executions
            .cancel_execution()
            .set_name(execution)
            .send()
            .await
            .context("cloud run cancel_execution")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cfg() -> CloudRunConfig {
        CloudRunConfig {
            project: "acme".into(),
            region: "europe-west1".into(),
            job: "cica-worker".into(),
            container_name: None,
            poll_interval_secs: 0, // no real waiting in tests
            timeout_secs: 60,
        }
    }

    struct FakeRun {
        statuses: Mutex<VecDeque<ExecutionStatus>>,
        run_ok: bool,
        cancel_called: AtomicBool,
    }
    impl FakeRun {
        fn new(statuses: Vec<ExecutionStatus>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into()),
                run_ok: true,
                cancel_called: AtomicBool::new(false),
            }
        }
        fn failing_run() -> Self {
            Self {
                statuses: Mutex::new(VecDeque::new()),
                run_ok: false,
                cancel_called: AtomicBool::new(false),
            }
        }
    }
    #[async_trait]
    impl CloudRunClient for FakeRun {
        async fn run_job(&self, _req: &RunJobRequest) -> Result<String> {
            if self.run_ok {
                Ok("projects/acme/locations/europe-west1/jobs/cica-worker/executions/e1".into())
            } else {
                anyhow::bail!("run_job failed")
            }
        }
        async fn get_execution(&self, _e: &str) -> Result<ExecutionStatus> {
            let mut q = self.statuses.lock().unwrap();
            if q.len() > 1 {
                Ok(q.pop_front().unwrap())
            } else {
                Ok(q.front().cloned().unwrap())
            }
        }
        async fn cancel_execution(&self, _e: &str) -> Result<()> {
            self.cancel_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    fn st(terminal: bool, succeeded: bool) -> ExecutionStatus {
        ExecutionStatus {
            terminal,
            succeeded,
            reason: None,
        }
    }

    #[test]
    fn job_name_is_fully_qualified() {
        assert_eq!(
            job_name("acme", "europe-west1", "cica-worker"),
            "projects/acme/locations/europe-west1/jobs/cica-worker"
        );
    }

    #[test]
    fn run_job_request_carries_turn_args() {
        let l = CloudRunLauncher::with_client(Box::new(FakeRun::new(vec![])), cfg());
        let req = l.run_job_request("turn-123");
        assert_eq!(req.project, "acme");
        assert_eq!(req.region, "europe-west1");
        assert_eq!(req.job, "cica-worker");
        assert!(req.container_name.is_none());
        assert_eq!(req.args, vec!["worker", "--turn", "turn-123"]);
    }

    #[tokio::test]
    async fn launch_ok_when_execution_succeeds() {
        let fake = FakeRun::new(vec![st(false, false), st(true, true)]);
        let l = CloudRunLauncher::with_client(Box::new(fake), cfg());
        assert!(l.launch("t1").await.is_ok());
    }

    #[tokio::test]
    async fn launch_errors_when_execution_fails() {
        let fake = FakeRun::new(vec![st(true, false)]);
        let l = CloudRunLauncher::with_client(Box::new(fake), cfg());
        assert!(l.launch("t1").await.is_err());
    }

    #[tokio::test]
    async fn launch_errors_when_run_job_fails() {
        let l = CloudRunLauncher::with_client(Box::new(FakeRun::failing_run()), cfg());
        assert!(l.launch("t1").await.is_err());
    }

    #[tokio::test]
    async fn launch_times_out_and_cancels() {
        let mut c = cfg();
        c.timeout_secs = 0;
        let fake = std::sync::Arc::new(FakeRun::new(vec![st(false, false)]));
        struct ArcRun(std::sync::Arc<FakeRun>);
        #[async_trait]
        impl CloudRunClient for ArcRun {
            async fn run_job(&self, r: &RunJobRequest) -> Result<String> {
                self.0.run_job(r).await
            }
            async fn get_execution(&self, e: &str) -> Result<ExecutionStatus> {
                self.0.get_execution(e).await
            }
            async fn cancel_execution(&self, e: &str) -> Result<()> {
                self.0.cancel_execution(e).await
            }
        }
        let l = CloudRunLauncher::with_client(Box::new(ArcRun(fake.clone())), c);
        assert!(l.launch("t1").await.is_err());
        assert!(fake.cancel_called.load(Ordering::SeqCst));
    }
}
