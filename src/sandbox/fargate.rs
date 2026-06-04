//! Fargate launcher (feature `fargate`).
//!
//! Implements `Launcher` via `ecs:RunTask` + a `DescribeTasks` poll, reusing
//! the store-mediated job/result protocol (the `TurnJob` is already in S3; only
//! the small `turn_id` rides the launch as a container command override). AWS
//! calls sit behind the `EcsClient` trait so the launch/poll/stop state machine
//! is testable without AWS; `aws-sdk-ecs` lives only in `AwsEcsClient`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::OnceCell;
use tokio::time::{Duration, Instant, sleep};

use crate::config::FargateConfig;
use crate::sandbox::worker::Launcher;

/// A request to start one worker task. Pure data, built from config + turn id.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // wired in Task 5
pub(crate) struct RunTaskRequest {
    pub cluster: String,
    pub task_definition: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    pub container_name: String,
    pub command: Vec<String>,
}

/// A task's observed status (subset DescribeTasks gives us).
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired in Task 5
pub(crate) struct TaskStatus {
    pub last_status: String,
    pub exit_code: Option<i32>,
    pub stopped_reason: Option<String>,
}

/// What `FargateLauncher` needs from ECS — the seam keeping aws-sdk-ecs at the edge.
#[async_trait]
#[allow(dead_code)] // wired in Task 5
pub(crate) trait EcsClient: Send + Sync {
    /// Start a task; returns its ARN. Errors if RunTask reports failures.
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String>;
    /// Current status of a task.
    async fn describe_task(&self, cluster: &str, task_arn: &str) -> Result<TaskStatus>;
    /// Best-effort stop (on timeout). The caller logs errors; not fatal.
    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()>;
}

/// Real `EcsClient` over aws-sdk-ecs, with a lazily-built client.
#[allow(dead_code)] // wired in Task 5
pub(crate) struct AwsEcsClient {
    region: Option<String>,
    client: OnceCell<aws_sdk_ecs::Client>,
}

impl AwsEcsClient {
    #[allow(dead_code)] // wired in Task 5
    pub(crate) fn new(region: Option<String>) -> Self {
        Self {
            region,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> Result<&aws_sdk_ecs::Client> {
        self.client
            .get_or_try_init(|| async {
                let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                if let Some(r) = &self.region {
                    loader = loader.region(aws_config::Region::new(r.clone()));
                }
                let shared = loader.load().await;
                Ok::<_, anyhow::Error>(aws_sdk_ecs::Client::new(&shared))
            })
            .await
    }
}

#[async_trait]
impl EcsClient for AwsEcsClient {
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String> {
        use aws_sdk_ecs::types::{
            AssignPublicIp, AwsVpcConfiguration, ContainerOverride, LaunchType,
            NetworkConfiguration, TaskOverride,
        };

        let assign = if req.assign_public_ip {
            AssignPublicIp::Enabled
        } else {
            AssignPublicIp::Disabled
        };
        let vpc = AwsVpcConfiguration::builder()
            .set_subnets(Some(req.subnets.clone()))
            .set_security_groups(Some(req.security_groups.clone()))
            .assign_public_ip(assign)
            .build()
            .context("building awsvpc configuration")?;
        let net = NetworkConfiguration::builder()
            .awsvpc_configuration(vpc)
            .build();
        let overrides = TaskOverride::builder()
            .container_overrides(
                ContainerOverride::builder()
                    .name(req.container_name.clone())
                    .set_command(Some(req.command.clone()))
                    .build(),
            )
            .build();

        let resp = self
            .client()
            .await?
            .run_task()
            .cluster(&req.cluster)
            .task_definition(&req.task_definition)
            .launch_type(LaunchType::Fargate)
            .network_configuration(net)
            .overrides(overrides)
            .send()
            .await
            .context("ecs run_task")?;

        if let Some(f) = resp.failures().first() {
            bail!(
                "ecs run_task failure: arn={:?} reason={:?} detail={:?}",
                f.arn(),
                f.reason(),
                f.detail()
            );
        }
        resp.tasks()
            .first()
            .and_then(|t| t.task_arn())
            .map(|s| s.to_string())
            .context("ecs run_task returned no task arn")
    }

    async fn describe_task(&self, cluster: &str, task_arn: &str) -> Result<TaskStatus> {
        let resp = self
            .client()
            .await?
            .describe_tasks()
            .cluster(cluster)
            .tasks(task_arn)
            .send()
            .await
            .context("ecs describe_tasks")?;
        let task = resp
            .tasks()
            .first()
            .context("ecs describe_tasks returned no task")?;
        let exit_code = task.containers().first().and_then(|c| c.exit_code());
        Ok(TaskStatus {
            last_status: task.last_status().unwrap_or("UNKNOWN").to_string(),
            exit_code,
            stopped_reason: task.stopped_reason().map(|s| s.to_string()),
        })
    }

    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()> {
        self.client()
            .await?
            .stop_task()
            .cluster(cluster)
            .task(task_arn)
            .reason(reason)
            .send()
            .await
            .context("ecs stop_task")?;
        Ok(())
    }
}

/// Launches a worker turn as a one-shot Fargate task.
#[allow(dead_code)] // wired in Task 5
pub struct FargateLauncher {
    ecs: Box<dyn EcsClient>,
    config: FargateConfig,
}

impl FargateLauncher {
    /// Build the AWS-backed launcher (lazy ECS client).
    #[allow(dead_code)] // wired in Task 5
    pub fn new(config: FargateConfig) -> Self {
        let region = config.region.clone();
        Self::with_client(Box::new(AwsEcsClient::new(region)), config)
    }

    /// Construct with an explicit `EcsClient` (used by `new` and by tests).
    #[allow(dead_code)] // wired in Task 5
    pub(crate) fn with_client(ecs: Box<dyn EcsClient>, config: FargateConfig) -> Self {
        Self { ecs, config }
    }

    /// The RunTask request for `turn_id`. Pure, for testing.
    #[allow(dead_code)] // wired in Task 5
    pub(crate) fn run_task_request(&self, turn_id: &str) -> RunTaskRequest {
        RunTaskRequest {
            cluster: self.config.cluster.clone(),
            task_definition: self.config.task_definition.clone(),
            subnets: self.config.subnets.clone(),
            security_groups: self.config.security_groups.clone(),
            assign_public_ip: self.config.assign_public_ip,
            container_name: self.config.container_name.clone(),
            command: vec!["worker".into(), "--turn".into(), turn_id.into()],
        }
    }
}

#[async_trait]
impl Launcher for FargateLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let arn = self.ecs.run_task(&self.run_task_request(turn_id)).await?;
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_secs);
        let interval = Duration::from_secs(self.config.poll_interval_secs);
        loop {
            let st = self.ecs.describe_task(&self.config.cluster, &arn).await?;
            if st.last_status == "STOPPED" {
                return match st.exit_code {
                    Some(0) => Ok(()),
                    other => bail!(
                        "worker task stopped (exit {other:?}, reason {:?})",
                        st.stopped_reason
                    ),
                };
            }
            if Instant::now() >= deadline {
                if let Err(e) = self
                    .ecs
                    .stop_task(&self.config.cluster, &arn, "cica turn timeout")
                    .await
                {
                    tracing::warn!("failed to stop timed-out task {arn}: {e}");
                }
                bail!("worker task timed out after {}s", self.config.timeout_secs);
            }
            sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cfg() -> FargateConfig {
        FargateConfig {
            cluster: "cica".into(),
            task_definition: "cica-worker".into(),
            subnets: vec!["subnet-a".into()],
            security_groups: vec!["sg-1".into()],
            assign_public_ip: false,
            region: None,
            container_name: "cica-worker".into(),
            poll_interval_secs: 0, // no real waiting in tests
            timeout_secs: 60,
        }
    }

    struct FakeEcs {
        statuses: Mutex<VecDeque<TaskStatus>>,
        run_ok: bool,
        stop_called: AtomicBool,
    }
    impl FakeEcs {
        fn new(statuses: Vec<TaskStatus>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into()),
                run_ok: true,
                stop_called: AtomicBool::new(false),
            }
        }
    }
    #[async_trait]
    impl EcsClient for FakeEcs {
        async fn run_task(&self, _req: &RunTaskRequest) -> Result<String> {
            if self.run_ok {
                Ok("arn:task/1".into())
            } else {
                bail!("run_task failed")
            }
        }
        async fn describe_task(&self, _c: &str, _a: &str) -> Result<TaskStatus> {
            let mut q = self.statuses.lock().unwrap();
            // Repeat the last scripted status once the queue drains.
            if q.len() > 1 {
                Ok(q.pop_front().unwrap())
            } else {
                Ok(q.front().cloned().unwrap())
            }
        }
        async fn stop_task(&self, _c: &str, _a: &str, _r: &str) -> Result<()> {
            self.stop_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn status(last: &str, exit: Option<i32>) -> TaskStatus {
        TaskStatus {
            last_status: last.into(),
            exit_code: exit,
            stopped_reason: None,
        }
    }

    #[test]
    fn run_task_request_carries_turn_command_and_network() {
        let l = FargateLauncher::with_client(Box::new(FakeEcs::new(vec![])), cfg());
        let req = l.run_task_request("turn-123");
        assert_eq!(req.cluster, "cica");
        assert_eq!(req.task_definition, "cica-worker");
        assert_eq!(req.subnets, vec!["subnet-a"]);
        assert_eq!(req.security_groups, vec!["sg-1"]);
        assert!(!req.assign_public_ip);
        assert_eq!(req.container_name, "cica-worker");
        assert_eq!(req.command, vec!["worker", "--turn", "turn-123"]);
    }

    #[tokio::test]
    async fn launch_ok_when_task_stops_zero() {
        let fake = FakeEcs::new(vec![status("RUNNING", None), status("STOPPED", Some(0))]);
        let l = FargateLauncher::with_client(Box::new(fake), cfg());
        assert!(l.launch("t1").await.is_ok());
    }

    #[tokio::test]
    async fn launch_errors_when_task_stops_nonzero() {
        let fake = FakeEcs::new(vec![status("STOPPED", Some(1))]);
        let l = FargateLauncher::with_client(Box::new(fake), cfg());
        assert!(l.launch("t1").await.is_err());
    }

    #[tokio::test]
    async fn launch_times_out_and_stops_task() {
        let mut c = cfg();
        c.timeout_secs = 0; // first not-stopped poll is already past deadline
        let fake = std::sync::Arc::new(FakeEcs::new(vec![status("RUNNING", None)]));
        struct ArcEcs(std::sync::Arc<FakeEcs>);
        #[async_trait]
        impl EcsClient for ArcEcs {
            async fn run_task(&self, r: &RunTaskRequest) -> Result<String> {
                self.0.run_task(r).await
            }
            async fn describe_task(&self, c: &str, a: &str) -> Result<TaskStatus> {
                self.0.describe_task(c, a).await
            }
            async fn stop_task(&self, c: &str, a: &str, r: &str) -> Result<()> {
                self.0.stop_task(c, a, r).await
            }
        }
        let l = FargateLauncher::with_client(Box::new(ArcEcs(fake.clone())), c);
        assert!(l.launch("t1").await.is_err());
        assert!(fake.stop_called.load(Ordering::SeqCst));
    }
}
