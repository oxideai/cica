# Phase 3b-2b: `FargateLauncher` + cloud worker config/secrets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run an agent turn on AWS Fargate via a feature-gated `FargateLauncher` (implements the existing `Launcher` trait through `ecs:RunTask` + a `DescribeTasks` poll), plus an env-secret overlay so a Fargate task gets its credentials without a bind-mount or secrets-in-config.

**Architecture:** `FargateLauncher` reuses the store-mediated job/result protocol (job already in S3; only the small `turn_id` rides the launch as a container command override). AWS calls sit behind a 3-method `EcsClient` trait so the launch/poll/stop state machine is unit-tested with a fake and `aws-sdk-ecs` stays at the edge (lazy `OnceCell` client, mirroring `S3StateStore`). Secrets reach the worker via a `CICA_*_API_KEY` env overlay applied in `Config::load()`; AWS creds come from the task IAM role. The first real `RunTask` is deferred to 3b-2c (with the `sprout` cluster) — this phase ships unit-tested.

**Tech Stack:** Rust 2024, `aws-config` + `aws-sdk-ecs` (optional, feature `fargate` which also enables `s3`), `tokio` (`OnceCell`, `time`), `async-trait`, `anyhow`.

---

## Why this is safe and incremental

Everything is additive and feature-gated. The default `cargo build` / `install.sh` pull no AWS SDK. The env overlay is plain always-compiled config logic (no dep) that benefits local/Docker/Fargate uniformly. `LaunchedWorkerProvider` and the per-turn dispatch path are unchanged — `FargateLauncher` is just another `Launcher`. With `provider` unset/`local`/`docker`, behavior is exactly as today.

## Background facts (verified against the codebase)

- `src/config.rs`:
  - `enum ProviderKind { Local, Subprocess, Docker }` with `#[serde(rename_all = "lowercase")]` and `derive(... PartialEq, Eq)` (line ~135). Adding `Fargate` → TOML `"fargate"`.
  - `S3Config` (line ~146) is the template for `FargateConfig` (always-compiled config struct; only the impl is feature-gated).
  - `DeploymentConfig` (line ~162) has `store`/`state_path`/`provider`/`docker_image`/`s3` fields, each `#[serde(default)]`.
  - `cursor: CursorConfig` and `claude: ClaudeConfig` are **non-Option** fields; each has `pub api_key: Option<String>` (lines ~337, ~356). Backends read `config.cursor.api_key` / `config.claude.api_key`.
  - `Config::load()` (line ~367): reads the file, `toml::from_str`, returns `Ok(config)`. The env overlay is applied here before returning.
- `src/sandbox/mod.rs`: `try_default_provider` matches `config.deployment.provider.unwrap_or(Local)` with `Local`/`Subprocess`/`Docker` arms; `Docker` requires a store. `state::default_store(config)?` is sync. The `s3` module is declared `#[cfg(feature = "s3")] pub mod s3;` in `src/sandbox/state/mod.rs` — mirror that for `fargate` in `src/sandbox/mod.rs`.
- `src/sandbox/worker.rs`: the `Launcher` trait is `async fn launch(&self, turn_id: &str) -> Result<()>`; `LaunchedWorkerProvider::new(store, Box<dyn Launcher>)`. `DockerLauncher::run_args` + its `docker_launcher_builds_run_args` test are the model for the pure `run_task_request` + its test.
- `Cargo.toml`: `[features] default = []` and `s3 = ["dep:aws-config", "dep:aws-sdk-s3"]` already exist; `aws-config`/`aws-sdk-s3` are optional. `aws-sdk-s3 = 1.119`, `aws-config = 1.8.14` are locked.

## File structure

- Modify `src/config.rs` — env overlay (`apply_env_overlay`/`overlay_secrets_from`) + `Config::load()` call; later `FargateConfig` + `ProviderKind::Fargate` + `fargate` field + `default_*` fns.
- Modify `Cargo.toml` — `[features] fargate = ["s3", "dep:aws-sdk-ecs"]` + optional `aws-sdk-ecs`.
- Create `src/sandbox/fargate.rs` (`#[cfg(feature = "fargate")]`) — `EcsClient` trait, `RunTaskRequest`, `TaskStatus`, `FargateLauncher` (poll loop + pure `run_task_request`), `AwsEcsClient` (real SDK impl), `FakeEcsClient` + tests.
- Modify `src/sandbox/mod.rs` — declare the gated `fargate` module + the `ProviderKind::Fargate` arm in `try_default_provider`.
- Modify `.github/workflows/ci.yml` — a `fargate` build + clippy lane.

---

### Task 1: Env-secret overlay (`CICA_*_API_KEY` → config)

**Files:**
- Modify: `src/config.rs`

The overlay core takes a lookup closure so tests never touch global env (no races).

- [ ] **Step 1: Write the failing tests**

In `src/config.rs`'s `#[cfg(test)] mod tests`:
```rust
    #[test]
    fn env_overlay_sets_cursor_and_claude_keys() {
        let mut cfg = Config::default();
        assert!(cfg.cursor.api_key.is_none());
        let env = |k: &str| match k {
            "CICA_CURSOR_API_KEY" => Some("cur-secret".to_string()),
            "CICA_CLAUDE_API_KEY" => Some("claude-secret".to_string()),
            _ => None,
        };
        cfg.overlay_secrets_from(env);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("cur-secret"));
        assert_eq!(cfg.claude.api_key.as_deref(), Some("claude-secret"));
    }

    #[test]
    fn env_overlay_leaves_config_value_when_env_absent() {
        let mut cfg = Config::default();
        cfg.cursor.api_key = Some("from-file".into());
        cfg.overlay_secrets_from(|_| None);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("from-file"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test config::tests::env_overlay`
Expected: FAIL — `overlay_secrets_from` not found.

- [ ] **Step 3: Implement the overlay**

In `impl Config` (near `load`), add:
```rust
    /// Overlay credential secrets from the process environment onto the loaded
    /// config. Lets cloud workers receive secrets via env (Secrets Manager →
    /// task env) instead of baking them into config.toml or the state store.
    pub(crate) fn apply_env_overlay(&mut self) {
        self.overlay_secrets_from(|k| std::env::var(k).ok());
    }

    /// Env overlay core, parameterized by a lookup so it is testable without
    /// touching the global process environment.
    fn overlay_secrets_from(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("CICA_CURSOR_API_KEY") {
            self.cursor.api_key = Some(v);
        }
        if let Some(v) = get("CICA_CLAUDE_API_KEY") {
            self.claude.api_key = Some(v);
        }
    }
```

And call it in `load()` before returning:
```rust
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Could not parse config file: {:?}", path))?;
        config.apply_env_overlay();
        Ok(config)
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test config::tests::env_overlay`
Expected: PASS.
Run: `cargo build` and `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` — clean. (`apply_env_overlay` is now called by `load`, so no dead_code.)

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
feat(config): overlay CICA_*_API_KEY secrets from env in Config::load

Lets cloud workers receive credentials via env (Secrets Manager → task
env) instead of baking them into config.toml or the state store.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `FargateConfig` + `[deployment.fargate]` field (no enum variant yet)

**Files:**
- Modify: `src/config.rs`

> Like phase 3b-2a, do NOT add the `ProviderKind::Fargate` variant here — it makes `try_default_provider`'s match non-exhaustive until Task 5 adds the arm. This task adds only the config struct + field, keeping the build green.

- [ ] **Step 1: Write the failing test**

In `src/config.rs`'s tests (does NOT reference `ProviderKind::Fargate`):
```rust
    #[test]
    fn deployment_fargate_section_parses_with_defaults() {
        let toml = r#"
            [deployment]
            [deployment.fargate]
            cluster = "cica"
            task_definition = "cica-worker"
            subnets = ["subnet-a", "subnet-b"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let f = cfg.deployment.fargate.unwrap();
        assert_eq!(f.cluster, "cica");
        assert_eq!(f.task_definition, "cica-worker");
        assert_eq!(f.subnets, vec!["subnet-a", "subnet-b"]);
        assert!(f.security_groups.is_empty());
        assert!(!f.assign_public_ip);
        assert_eq!(f.region, None);
        assert_eq!(f.container_name, "cica-worker");
        assert_eq!(f.poll_interval_secs, 5);
        assert_eq!(f.timeout_secs, 900);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::deployment_fargate_section_parses_with_defaults`
Expected: FAIL — `FargateConfig` / `fargate` field don't exist.

- [ ] **Step 3: Add the struct, default fns, and field**

Add near `S3Config`:
```rust
fn default_container_name() -> String {
    "cica-worker".to_string()
}
fn default_poll_interval_secs() -> u64 {
    5
}
fn default_timeout_secs() -> u64 {
    900
}

/// Fargate launcher settings (used when `provider = "fargate"`). Credentials
/// come from the task IAM role (the AWS chain), never config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FargateConfig {
    /// ECS cluster name or ARN (required).
    pub cluster: String,
    /// Task-definition family or `family:revision` (required).
    pub task_definition: String,
    /// awsvpc subnets to launch into (required in practice).
    #[serde(default)]
    pub subnets: Vec<String>,
    /// Security groups; default none.
    #[serde(default)]
    pub security_groups: Vec<String>,
    /// Assign a public IP (default false — private subnets + NAT).
    #[serde(default)]
    pub assign_public_ip: bool,
    /// AWS region; falls back to the default chain when unset.
    #[serde(default)]
    pub region: Option<String>,
    /// Which container in the task-def to override with `worker --turn <id>`.
    #[serde(default = "default_container_name")]
    pub container_name: String,
    /// DescribeTasks poll interval in seconds.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Max seconds to wait for the task to stop before bailing.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}
```

Add to `DeploymentConfig` (after `s3`):
```rust
    /// Fargate launcher settings (used when `provider = "fargate"`).
    #[serde(default)]
    pub fargate: Option<FargateConfig>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test config::tests::deployment_fargate_section_parses_with_defaults`
Expected: PASS. `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` — clean.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
feat(config): add FargateConfig + [deployment.fargate] section

The ProviderKind::Fargate variant lands later with its try_default_provider
arm to keep builds green.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `fargate` feature + `fargate.rs` core (trait, request, launcher poll loop, fake-backed tests)

**Files:**
- Modify: `Cargo.toml`
- Create: `src/sandbox/fargate.rs`
- Modify: `src/sandbox/mod.rs` (declare the gated module)

This task contains NO `aws-sdk-ecs` API usage — all of it (the `AwsEcsClient`) is Task 4. So `fargate.rs` here compiles purely against our own `EcsClient` trait + `tokio`, and the feature simply pulls the (as-yet-unused) `aws-sdk-ecs` crate.

- [ ] **Step 1: Cargo feature + optional dep**

In `Cargo.toml` `[dependencies]`, add (optional):
```toml
aws-sdk-ecs = { version = "1", optional = true }
```
In `[features]`, add (note: `fargate` enables `s3` — the cloud worker uses the S3 store):
```toml
fargate = ["s3", "dep:aws-sdk-ecs"]
```

- [ ] **Step 2: Declare the gated module in `src/sandbox/mod.rs`**

Near the other `mod` declarations (e.g. below `pub mod worker;`):
```rust
#[cfg(feature = "fargate")]
mod fargate;
```

- [ ] **Step 3: Write `src/sandbox/fargate.rs` core + fake-backed tests**

```rust
//! Fargate launcher (feature `fargate`).
//!
//! Implements `Launcher` via `ecs:RunTask` + a `DescribeTasks` poll, reusing
//! the store-mediated job/result protocol (the `TurnJob` is already in S3; only
//! the small `turn_id` rides the launch as a container command override). AWS
//! calls sit behind the `EcsClient` trait so the launch/poll/stop state machine
//! is testable without AWS; `aws-sdk-ecs` lives only in `AwsEcsClient`.

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::time::{Duration, Instant, sleep};

use crate::config::FargateConfig;
use crate::sandbox::worker::Launcher;

/// A request to start one worker task. Pure data, built from config + turn id.
#[derive(Debug, Clone, PartialEq)]
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
pub(crate) struct TaskStatus {
    pub last_status: String,
    pub exit_code: Option<i32>,
    pub stopped_reason: Option<String>,
}

/// What `FargateLauncher` needs from ECS — the seam keeping aws-sdk-ecs at the edge.
#[async_trait]
pub(crate) trait EcsClient: Send + Sync {
    /// Start a task; returns its ARN. Errors if RunTask reports failures.
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String>;
    /// Current status of a task.
    async fn describe_task(&self, cluster: &str, task_arn: &str) -> Result<TaskStatus>;
    /// Best-effort stop (on timeout). The caller logs errors; not fatal.
    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()>;
}

/// Launches a worker turn as a one-shot Fargate task.
pub struct FargateLauncher {
    ecs: Box<dyn EcsClient>,
    config: FargateConfig,
}

impl FargateLauncher {
    /// Construct with an explicit `EcsClient` (used by `new` and by tests).
    pub(crate) fn with_client(ecs: Box<dyn EcsClient>, config: FargateConfig) -> Self {
        Self { ecs, config }
    }

    /// The RunTask request for `turn_id`. Pure, for testing.
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
            let st = self
                .ecs
                .describe_task(&self.config.cluster, &arn)
                .await?;
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
        // Move a clone into the launcher; keep one to assert stop_called.
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
```

- [ ] **Step 4: Build + test with the feature**

Run: `cargo build --features fargate`
Expected: SUCCESS — this first pulls `aws-sdk-ecs` (slow; it shares the AWS runtime already locked by `s3`, so no new TLS surprise is expected — but if it fails for a dependency reason, resolve it as in 3b-2a and report). `aws-sdk-ecs` is unused so far; an enabled-but-unused optional dep does not warn.
Run: `cargo test --features fargate sandbox::fargate::tests`
Expected: the 4 tests pass.
Run: `cargo build` (no features) → succeeds and pulls no AWS crates.
Run: `cargo clippy --features fargate --all-targets -- -D warnings` and `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` — all clean.

> Note: `FargateLauncher::with_client` and the trait/types are `pub(crate)` and currently only used by tests + (next task) `new`/wiring. If clippy flags any item as dead under `--features fargate` before Task 5 wires it, add a narrowly-justified `#[allow(dead_code)]` ON THAT ITEM with a `// wired in Task 5` comment (NOT a module-wide allow); Task 5 removes it. Prefer to first check whether the tests already exercise it (they construct `FargateLauncher` and call `run_task_request`/`launch`, so most items are live).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/sandbox/fargate.rs src/sandbox/mod.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): fargate feature + FargateLauncher poll loop (EcsClient seam)

Launch/poll/stop state machine behind a testable EcsClient trait; the real
aws-sdk-ecs impl lands next. Fake-backed unit tests cover ok/nonzero/timeout.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `AwsEcsClient` — the real `aws-sdk-ecs` impl + `FargateLauncher::new`

**Files:**
- Modify: `src/sandbox/fargate.rs`

> Targets `aws-sdk-ecs` 1.x. The exact builder shapes may differ; after writing, run `cargo build --features fargate` and adjust to compile while preserving behavior. Report every deviation. Likely verification points are called out inline.

- [ ] **Step 1: Add imports + the lazy client + `AwsEcsClient`**

Add to the top of `src/sandbox/fargate.rs`:
```rust
use anyhow::Context;
use tokio::sync::OnceCell;
```

Add the real client (after the `EcsClient` trait):
```rust
/// Real `EcsClient` over aws-sdk-ecs, with a lazily-built client.
pub(crate) struct AwsEcsClient {
    region: Option<String>,
    client: OnceCell<aws_sdk_ecs::Client>,
}

impl AwsEcsClient {
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
```

- [ ] **Step 2: Implement `EcsClient` for `AwsEcsClient`**

```rust
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
                    .name(&req.container_name)
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
        // The worker is the only container we care about; take its exit code.
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
```

> Verify against aws-sdk-ecs 1.x:
> - `AwsVpcConfiguration::builder().build()` — may return `Result` (subnets is required) → `?` as written; if it returns the value directly, drop the `.context(..)?`.
> - `NetworkConfiguration::builder().build()` / `ContainerOverride::builder().build()` / `TaskOverride::builder().build()` — likely return the value directly (optional fields). Adjust if any returns `Result`.
> - `resp.failures()` / `resp.tasks()` return `&[T]` (slices) in recent SDKs. `Failure::arn/reason/detail`, `Task::task_arn/last_status/stopped_reason/containers`, `Container::exit_code() -> Option<i32>`. Adjust if any is `Option<&[T]>`.
> - `LaunchType::Fargate`, `AssignPublicIp::{Enabled,Disabled}` enum paths under `aws_sdk_ecs::types`.

- [ ] **Step 3: Add `FargateLauncher::new`**

In `impl FargateLauncher`, add the real constructor:
```rust
    /// Build the AWS-backed launcher (lazy ECS client).
    pub fn new(config: FargateConfig) -> Self {
        let region = config.region.clone();
        Self::with_client(Box::new(AwsEcsClient::new(region)), config)
    }
```

- [ ] **Step 4: Build + test**

Run: `cargo build --features fargate` → SUCCESS (fix SDK API mismatches per the notes; report deviations).
Run: `cargo test --features fargate sandbox::fargate::tests` → the 4 fake-backed tests still pass (no AWS needed; `AwsEcsClient` has no unit test — its real behavior is covered by 3b-2c).
Run: `cargo build` (default) → succeeds.
Run: `cargo clippy --features fargate --all-targets -- -D warnings` and `cargo fmt --check` — clean. (`AwsEcsClient`/`new` may still be dead until Task 5 wires `new`; if clippy flags them, add a per-item `#[allow(dead_code)] // wired in Task 5` and remove it in Task 5.)

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/fargate.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): AwsEcsClient (real aws-sdk-ecs RunTask/DescribeTasks/StopTask)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Wire `ProviderKind::Fargate` + `try_default_provider` arm

**Files:**
- Modify: `src/config.rs`
- Modify: `src/sandbox/mod.rs`
- Modify: `src/sandbox/fargate.rs` (remove any dead_code allows added in 3/4)

- [ ] **Step 1: Add the `Fargate` variant**

In `src/config.rs`, add `Fargate` to `ProviderKind`:
```rust
pub enum ProviderKind {
    Local,
    Subprocess,
    Docker,
    Fargate,
}
```
(`#[serde(rename_all = "lowercase")]` → `"fargate"`.)

- [ ] **Step 2: Add a parse test (config.rs)**

```rust
    #[test]
    fn provider_parses_fargate() {
        let toml = r#"
            [deployment]
            provider = "fargate"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Fargate));
    }
```

- [ ] **Step 3: Add the `try_default_provider` arm (`src/sandbox/mod.rs`)**

Add to the `match`:
```rust
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
```
(The `store` binding is moved/consumed inside the feature block; the `let _ = store;` in the non-feature block silences the unused warning. If clippy is clean without it, omit it.)

- [ ] **Step 4: Remove dead_code allows in `fargate.rs`**

If Tasks 3/4 added any `#[allow(dead_code)] // wired in Task 5`, remove them now — `FargateLauncher::new` (and transitively `AwsEcsClient`) is reachable from the wiring under `--features fargate`.

- [ ] **Step 5: Add wiring tests (`src/sandbox/mod.rs`)**

In the `#[cfg(test)] mod tests`:
```rust
    #[cfg(not(feature = "fargate"))]
    #[test]
    fn fargate_provider_requires_feature() {
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-fargate-test".into());
        // Feature off → must error even though a store is present.
        assert!(try_default_provider(&cfg).is_err());
    }

    #[test]
    fn fargate_provider_requires_a_store() {
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Fargate);
        assert!(try_default_provider(&cfg).is_err());
    }

    #[cfg(feature = "fargate")]
    #[test]
    fn fargate_provider_built_when_feature_and_store_and_section() {
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
        assert!(try_default_provider(&cfg).is_ok());
    }
```
(`FargateConfig::default()` gives empty container_name/0 timeouts via `Default`, which is fine for construction — the launcher only connects on `launch`. The `container_name`/poll defaults from serde apply on parse, not `Default`; that's acceptable since this test constructs directly and never launches.)

- [ ] **Step 6: Build + test both feature sets**

Run: `cargo build && cargo test` (default) → compiles (match exhaustive), all pass incl. `fargate_provider_requires_feature` + `provider_parses_fargate`.
Run: `cargo build --features fargate && cargo test --features fargate` → all pass incl. `fargate_provider_built_when_feature_and_store_and_section`.
Run: `cargo clippy --all-targets -- -D warnings`, `cargo clippy --features fargate --all-targets -- -D warnings`, `cargo fmt --check` — all clean.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/sandbox/mod.rs src/sandbox/fargate.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): select FargateLauncher via provider = fargate (feature-gated)

Adds ProviderKind::Fargate + the try_default_provider arm (requires a store;
fails fast without --features fargate).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: CI `fargate` lane + worker-contract doc note

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `docs/superpowers/specs/2026-06-03-phase3b1-container-worker-design.md` (append a short cloud-contract note) — OR a dedicated `docs/worker-contract.md` if one exists; check first and prefer the existing contract location.

- [ ] **Step 1: Add a fargate build+clippy step to CI**

Fargate can't run in CI (no emulator), so we only compile + lint the feature. Add a step to the existing `s3-store` job (it already installs Rust with clippy), after the s3 clippy step:
```yaml
      - name: Clippy (fargate feature)
        run: cargo clippy --features fargate --all-targets -- -D warnings
```
(If you prefer isolation, add a tiny standalone job instead; folding into `s3-store` reuses the toolchain setup and is enough. `--features fargate` also enables `s3`, so it covers both.)

- [ ] **Step 2: Validate YAML**

Run: `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/ci.yml')); print(any('fargate' in str(s) for j in d['jobs'].values() for s in j.get('steps', [])))"`
Expected: `True`.

- [ ] **Step 3: Append the cloud worker-contract note**

In the 3b-1 contract doc (`docs/superpowers/specs/2026-06-03-phase3b1-container-worker-design.md`), under the worker-contract section, append a short subsection documenting the cloud delta (so sprout has the authoritative list). Keep it factual:
```markdown
### Cloud (Fargate) worker contract delta (3b-2b)

A Fargate task has no bind mounts, so on top of the base contract:
- **Command override:** the launcher overrides the named container's command to `worker --turn <id>` per turn; the task-def's default command is irrelevant. The task-def must name the worker container per `[deployment.fargate].container_name` (default `cica-worker`).
- **Non-secret config:** sprout supplies `/data/cica/config.toml` (baked into a derived image or written by an entrypoint) with `backend`, `[deployment] store = "s3"`, and `[deployment.s3]`.
- **Secrets:** injected as env from Secrets Manager — `CICA_CURSOR_API_KEY` and/or `CICA_CLAUDE_API_KEY`. cica overlays them onto the loaded config. Never in the image/S3/file.
- **AWS credentials:** the task IAM role (S3 state bucket access). The router's role needs `ecs:RunTask`, `ecs:DescribeTasks`, `ecs:StopTask`, and `iam:PassRole` for the task/execution roles.
```

- [ ] **Step 4: Final gates**

Run: `cargo clippy --all-targets -- -D warnings` (default) and `cargo clippy --features fargate --all-targets -- -D warnings` — clean.
Run: `cargo fmt --check` — clean.
Run: `cargo test` (default) and `cargo test --features fargate` — all pass.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml docs/superpowers/specs/2026-06-03-phase3b1-container-worker-design.md
git commit -m "$(cat <<'EOF'
ci+docs: fargate clippy lane + cloud worker-contract delta

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- Feature `fargate = ["s3", "dep:aws-sdk-ecs"]`, optional dep → Task 3.
- `FargateConfig` (cluster/task-def/subnets/sgs/assign_public_ip/region/container_name/poll/timeout) + `[deployment.fargate]` → Task 2.
- `ProviderKind::Fargate` + feature-gated `try_default_provider` arm + fail-fast + requires-store → Task 5.
- `EcsClient` 3-method seam (`run_task`/`describe_task`/`stop_task`); `aws-sdk-ecs` only in `AwsEcsClient`; lazy `OnceCell` client → Tasks 3 (trait + launcher), 4 (real impl).
- `RunTask` with launch_type/awsvpc/container command override; `failures[]` → bail; task ARN extraction → Task 4; pure `run_task_request` → Task 3.
- Poll `DescribeTasks` until STOPPED; exit 0 → Ok else Err; **best-effort `StopTask` on timeout** → Task 3 (loop) + 4 (real stop).
- Env-secret overlay (`CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY`) in `Config::load`, race-free via lookup closure → Task 1.
- Cloud worker-contract doc delta → Task 6.
- Testing: pure `run_task_request` test, fake-backed poll state machine (ok/nonzero/timeout+stop), env-overlay unit tests, fail-fast wiring tests; CI `fargate` clippy lane; real RunTask deferred to 3b-2c → Tasks 1,3,5,6.
- Distribution: default build pulls no AWS SDK; `--features fargate` adds ecs(+s3) → Tasks 3, 6.

**Placeholder scan:** No "TBD"/"handle appropriately". The aws-sdk-ecs builder/Result-shape notes (Task 4) are explicit "verify against the installed 1.x and adjust, preserving behavior" guidance for the genuinely SDK-version-dependent surface — the same honest pattern used for `S3StateStore` (3b-2a) and the Dockerfile (3b-1) — not placeholders for logic.

**Type consistency:** `EcsClient` (`run_task(&RunTaskRequest)->Result<String>`, `describe_task(&str,&str)->Result<TaskStatus>`, `stop_task(&str,&str,&str)->Result<()>`) is identical across the trait (Task 3), `FakeEcs` (Task 3), and `AwsEcsClient` (Task 4). `FargateLauncher::with_client(Box<dyn EcsClient>, FargateConfig)` + `new(FargateConfig)` consistent across Tasks 3/4/5. `RunTaskRequest`/`TaskStatus` fields match between the launcher, the fake, and the real impl. `FargateConfig` fields match config (Task 2), the launcher (Task 3), wiring (Task 5), and the parse test. `overlay_secrets_from`/`apply_env_overlay` consistent between definition and `load` call (Task 1). `ProviderKind::Fargate` serde-maps to `"fargate"` (Task 5), matching the parse test.

## Next (after this merges)

Phase 3b-2c: the `sprout` CDK (ECR + image push, S3 state bucket, ECS cluster + worker task-def naming the `cica-worker` container + the env-secret wiring, task/execution IAM roles, the router's `ecs:*`/`iam:PassRole` role, networking) — and the **first real `RunTask` end-to-end** on Fargate, validating cica + sprout together.
