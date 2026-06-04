# Phase 3b-2b: `FargateLauncher` + cloud worker config/secrets contract

**Date:** 2026-06-04
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 3b-1 (`Launcher` trait + `DockerLauncher` + worker contract), Phase 3b-2a (feature-gated `S3StateStore`).

## Goal

Run an agent turn on **AWS Fargate**: a `FargateLauncher` that implements the existing `Launcher` trait via `ecs:RunTask` + a `DescribeTasks` poll, reusing the store-mediated job/result protocol over `S3StateStore`. Plus the **cloud worker config/secrets contract** — how a Fargate task (which has no host filesystem to bind-mount) gets its config and credentials. Feature-gated so the default build stays lean. The real `RunTask`-against-Fargate validation is deferred to 3b-2c (joint with the `sprout` CDK that stands up the cluster); 3b-2b ships unit-tested with the AWS calls behind a small testable seam.

## Where this fits (Phase 3b-2 decomposition)

- **3b-2a (done, PR #11):** `S3StateStore` (feature `s3`).
- **3b-2b (this spec):** `FargateLauncher` (feature `fargate`) + the env-secret overlay + worker-contract doc for cloud.
- **3b-2c:** the `sprout` CDK (ECR push, S3 bucket, ECS cluster + task-def, IAM roles, networking, secrets) — where the first real `RunTask` runs end-to-end.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Dependency | `aws-sdk-ecs`, **optional**, behind `[features] fargate = ["s3", ...]` | Fargate implies the S3 store; default build stays lean. |
| Trait | Implement the existing `Launcher` (`launch(turn_id)`) | `LaunchedWorkerProvider` + store-mediated job/result are unchanged; only the launch step is Fargate-specific. |
| Job transport | Small `turn_id` as a container command override; large `TurnJob` via S3 (already pushed by `LaunchedWorkerProvider`) | Cloud-neutral; mirrors the 3a/3b-1 store-mediated protocol. No Step Functions / callback tokens. |
| AWS seam | A 3-method `EcsClient` trait (`run_task`/`describe_task`/`stop_task`); `aws-sdk-ecs` only in the real impl | The launch/poll/stop state machine is unit-testable with a fake; the SDK stays at the edge. |
| Config delivery | Worker reads non-secret `config.toml` (provided by sprout); secrets via an **env overlay**; AWS creds via the task IAM role | Secrets stay in Secrets Manager — never in the image, S3, or a file. Locked rule from 3b-2a. |
| Timeout handling | Best-effort `StopTask` on poll timeout, then bail | Never leak a running Fargate task (cost + a late S3 result write). Failure to stop is logged, not masked. |
| `assign_public_ip` | Default `false` | Typical sprout setup = private subnets + NAT egress. |
| Validation | Unit-tested now; real `RunTask` end-to-end in 3b-2c | Fargate has no local emulator (unlike S3/LocalStack); validate cica+sprout together. |
| Build-without-feature | `provider = "fargate"` + binary lacks `--features fargate` → fail fast | Same actionable error as `store = "s3"`. |

## Config

Add to `src/config.rs`:
- `ProviderKind::Fargate` (alongside `Local`, `Subprocess`, `Docker`).
- A `FargateConfig` struct on `DeploymentConfig` (always compiles; only the launcher impl is feature-gated, mirroring `S3Config`):

```toml
[deployment]
provider = "fargate"
store = "s3"          # required in practice

[deployment.fargate]
cluster = "cica"                                   # required
task_definition = "cica-worker"                    # required (family or family:revision)
subnets = ["subnet-aaa", "subnet-bbb"]             # required (awsvpc)
security_groups = ["sg-xxx"]                       # optional; default []
assign_public_ip = false                           # default false
region = "eu-west-1"                               # optional; falls back to the AWS chain
container_name = "cica-worker"                      # default; which container in the task-def to override
poll_interval_secs = 5                             # default 5
timeout_secs = 900                                 # default 900
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FargateConfig {
    pub cluster: String,
    pub task_definition: String,
    #[serde(default)]
    pub subnets: Vec<String>,
    #[serde(default)]
    pub security_groups: Vec<String>,
    #[serde(default)]
    pub assign_public_ip: bool,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default = "default_container_name")]
    pub container_name: String,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}
```
`DeploymentConfig` gains `#[serde(default)] pub fargate: Option<FargateConfig>`. The `default_*` fns supply `"cica-worker"`, `5`, `900`. `region` mirrors `S3Config`: the ECS client applies it when set, else falls back to the AWS default chain (sprout sets `AWS_REGION` on the router).

## Components

### `EcsClient` trait + `RunTaskRequest` (`src/sandbox/fargate.rs`)

```rust
/// What FargateLauncher needs from ECS — the seam that keeps aws-sdk-ecs at the edge.
#[async_trait]
trait EcsClient: Send + Sync {
    /// Start a task; returns its ARN. Errors if RunTask reports failures.
    async fn run_task(&self, req: &RunTaskRequest) -> Result<String>;
    /// Current status of a task.
    async fn describe_task(&self, cluster: &str, task_arn: &str) -> Result<TaskStatus>;
    /// Best-effort stop (on timeout). Errors are logged by the caller, not fatal.
    async fn stop_task(&self, cluster: &str, task_arn: &str, reason: &str) -> Result<()>;
}

struct RunTaskRequest {
    cluster: String,
    task_definition: String,
    subnets: Vec<String>,
    security_groups: Vec<String>,
    assign_public_ip: bool,
    container_name: String,
    command: Vec<String>,         // ["worker", "--turn", <id>]
}

struct TaskStatus {
    last_status: String,          // "PROVISIONING" | "PENDING" | "RUNNING" | "STOPPED" | ...
    exit_code: Option<i32>,       // the override container's exit code (once STOPPED)
    stopped_reason: Option<String>,
}
```

### `FargateLauncher` (`#[cfg(feature = "fargate")]`)

```rust
pub struct FargateLauncher {
    ecs: Box<dyn EcsClient>,
    config: FargateConfig,
}

impl FargateLauncher {
    /// Builds the real AWS-backed launcher (lazy client like S3StateStore).
    pub fn new(config: FargateConfig) -> Self { /* AwsEcsClient over aws-sdk-ecs */ }
    fn run_task_request(&self, turn_id: &str) -> RunTaskRequest { /* pure, testable */ }
}

#[async_trait]
impl Launcher for FargateLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let arn = self.ecs.run_task(&self.run_task_request(turn_id)).await?;
        let deadline = now + timeout;
        loop {
            let st = self.ecs.describe_task(&self.config.cluster, &arn).await?;
            if st.last_status == "STOPPED" {
                return match st.exit_code {
                    Some(0) => Ok(()),
                    other => bail!("worker task stopped (exit {other:?}, reason {:?})", st.stopped_reason),
                };
            }
            if past_deadline {
                let _ = self.ecs.stop_task(&cluster, &arn, "cica turn timeout").await
                    .map_err(|e| tracing::warn!("failed to stop timed-out task {arn}: {e}"));
                bail!("worker task timed out after {timeout_secs}s");
            }
            sleep(poll_interval).await;
        }
    }
}
```
- `AwsEcsClient` wraps `aws_sdk_ecs::Client`, built lazily via `OnceCell` (same pattern as `S3StateStore`) so construction stays cheap and the AWS config loads on first use. `run_task` maps `RunTaskRequest` → the SDK builder (`launch_type(FARGATE)`, `network_configuration(awsvpc{subnets, security_groups, assign_public_ip})`, `overrides(container_overrides[{name, command}])`) and returns `tasks[0].task_arn`, bailing if `failures` is non-empty. `describe_task` reads `tasks[0].last_status` + the named container's `exit_code`/`stopped_reason`.
- `now`/`sleep` use `tokio::time` (`Instant`, `sleep`) — not `Date::now`.

### Wiring — `try_default_provider` (`src/sandbox/mod.rs`)

```rust
ProviderKind::Fargate => {
    let store = store.ok_or_else(|| anyhow!("`provider = fargate` requires [deployment].store"))?;
    #[cfg(feature = "fargate")]
    {
        let fc = config.deployment.fargate.clone()
            .ok_or_else(|| anyhow!("`provider = fargate` requires a [deployment.fargate] section"))?;
        Ok(Box::new(worker::LaunchedWorkerProvider::new(store, Box::new(fargate::FargateLauncher::new(fc)))))
    }
    #[cfg(not(feature = "fargate"))]
    { anyhow::bail!("`provider = fargate` requires the binary to be built with `--features fargate`") }
}
```
`fargate` module declared `#[cfg(feature = "fargate")] mod fargate;` in `src/sandbox/mod.rs`. The arm requires a store (as Docker/Subprocess do); in practice that store is S3 — a filesystem store would not be reachable from a Fargate task (documented, not code-enforced, since the launcher doesn't inspect the store kind).

## The cloud worker config/secrets contract (the deliverable sprout targets)

Extends the 3b-1 worker contract for cloud. A Fargate task has no bind mounts, so:

1. **Non-secret config** — the worker reads `/data/cica/config.toml` (`backend`, `[deployment] store = "s3"`, `[deployment.s3]` bucket/region) exactly as locally. In cloud, **sprout provides this file** (baked into a thin derived image, or written by an entrypoint) — its delivery is sprout's concern; cica only requires the file to exist at that path.
2. **Secrets — env overlay (the cica-side deliverable).** After loading `config.toml`, cica overlays two env vars when set:
   - `CICA_CURSOR_API_KEY` → `config.cursor.api_key`
   - `CICA_CLAUDE_API_KEY` → `config.claude.api_key`
   sprout wires these from Secrets Manager into the task-def's container secrets. They never appear in the image, S3, or a file.
3. **AWS credentials** for the S3 store come from the **task IAM role** (the standard AWS provider chain) — no env, no config. The router's role needs `ecs:RunTask`, `ecs:DescribeTasks`, `ecs:StopTask`, and `iam:PassRole` for the task/execution roles.
4. **cwd pinned to `/data/cica`** (image `ENV XDG_CONFIG_HOME=/data`, from 3b-1) so the Cursor workspace hash matches existing prod sessions.
5. The container's command is **overridden per turn** to `worker --turn <id>` (the task-def's default command is irrelevant). sprout's task-def must name the worker container `container_name` (default `cica-worker`).

The env overlay is applied in config loading so it benefits every provider uniformly (local, docker, fargate) — running locally with `CICA_CURSOR_API_KEY` set works identically.

## Data flow (one turn, `provider = "fargate"`)

```
router → LaunchedWorkerProvider.run_turn(job)
  push turns/<id>/job → S3StateStore
  FargateLauncher.launch(<id>):
    ecs:RunTask(cluster, task-def, awsvpc{subnets,sgs,public_ip},
                overrides: container <name> command ["worker","--turn",<id>])  → task ARN
    poll ecs:DescribeTasks until lastStatus=STOPPED (or timeout → StopTask + bail)
      [Fargate task] XDG_CONFIG_HOME=/data → base=/data/cica
        cica worker: pull job ← S3 ; hydrate session+memories ← S3 into fresh homes
          run backend (cursor/claude; creds from CICA_*_API_KEY env)
          capture session + push, push memories → S3
        push turns/<id>/result → S3 ; exit 0
    container exit 0 → Ok
  pull turns/<id>/result ← S3 → TurnResult ; cleanup turns/<id>
→ router posts response
```

## Error handling

- **RunTask failures** (`failures[]` non-empty: capacity, subnet, image pull) → `launch` returns `Err` with the failure reason → turn error; blobs cleaned up by `LaunchedWorkerProvider`.
- **Poll timeout** → best-effort `StopTask` (warn on failure), then `Err`.
- **Container non-zero exit / no result** → `Err` (existing pipeline surfaces it; non-zero means no `result` blob was written).
- **DescribeTasks transient error** → propagate as `Err` (v1: no internal retry; the turn fails and the channel/cron retry path applies). Revisit if flakiness shows.
- **Missing `[deployment.fargate]` with `provider = "fargate"`** → clear config error.
- **`provider = "fargate"` without `--features fargate`** → fail fast at provider construction.

## Testing strategy

- **Unit (no AWS):**
  - `run_task_request(turn_id)` construction from `FargateConfig` — asserts cluster, task-def, subnets, security groups, `assign_public_ip`, container name, and `command == ["worker","--turn",<id>]`. (Mirrors `docker_launcher_builds_run_args`.)
  - Poll state machine with a scripted `FakeEcsClient`: `STOPPED+exit0 → Ok`; `STOPPED+exit1 → Err`; never-stops → timeout `Err` **and** `stop_task` was called (fake records the call).
  - Env-secret overlay: a config with no `cursor.api_key`, `CICA_CURSOR_API_KEY` set in-process → loaded config has the key; same for `CICA_CLAUDE_API_KEY` → `claude.api_key`. (Use a serialized guard / unique var handling so env tests don't race.)
- **Reuse the fake-backend path:** the existing `docker_flow_round_trips_with_fake_backend` already injects env via `with_env`; add a variant (or assertion) that a secret provided **only** via `CICA_CURSOR_API_KEY` env (config file omits it) is honored — proving the cloud secret path without real AWS. Gated by `CICA_DOCKER_IT` like today.
- **Real `RunTask` deferred to 3b-2c:** once sprout stands up the cluster/task-def, a gated end-to-end turn runs on real Fargate. No Fargate emulator exists, so there is **no** CI service-container test here (unlike S3/LocalStack).
- **CI:** a `fargate` build + clippy lane (`cargo clippy --features fargate --all-targets -- -D warnings`) so the feature code compiles + lints in CI even though it can't run. (Can be folded into the existing `s3-store` job or a small dedicated step.)

## Distribution impact

- Default `cargo build` + `install.sh` unchanged — `aws-sdk-ecs` is optional, pulled only by `--features fargate` (which also enables `s3`).
- The env overlay adds **no dependency** and is always compiled (it's plain config logic), so the local/Docker paths get it for free.
- 3b-2c / release: cloud artifacts build with `--features fargate`; the worker image (3b-1) is unchanged (the launcher is router-side).

## Out of scope (later)

- The `sprout` CDK (cluster, task-def, IAM, networking, secrets, ECR) and the first real `RunTask` → 3b-2c.
- GCP Cloud Run Jobs launcher + `GcsStateStore` → 3b-3.
- Warm/reused tasks, internal DescribeTasks retry/backoff tuning, `StopTask` on every error path (only timeout in v1), CloudWatch log wiring → revisit if needed.
