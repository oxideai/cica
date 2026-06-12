# Phase 3b-3: GCP worker fleet — `GcsStateStore` + `CloudRunLauncher`

**Date:** 2026-06-09
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** 3b-2a (`S3StateStore`), 3b-2b (`FargateLauncher`), 3b-2d (published worker image + env-driven config).

## Goal

Make the cica worker fleet run on **GCP** as a self-contained second deployment (for a different organization), with no change to the live AWS deployment. The whole job is two feature-gated primitives that mirror the AWS pair behind the existing `StateStore` / `Launcher` traits:

- **`GcsStateStore`** (feature `gcs`) — durable state on Google Cloud Storage, sibling to `S3StateStore`.
- **`CloudRunLauncher`** (feature `cloudrun`) — run a worker turn as a one-shot **Cloud Run Job execution**, sibling to `FargateLauncher`.

"Cloud is config, not a code fork" was the founding premise of these traits; this phase is where it gets cashed in for a second cloud. The first real `RunJob` against Cloud Run is the acceptance test on the **separate GCP IaC track** (mirrors how the first real Fargate `RunTask` was deferred to `sprout`); this phase ships unit-tested, with a gated `fake-gcs-server` integration test for the store.

## Scope

**In scope (this spec):** the two primitives + config/enum/env-overlay/feature wiring + CI lanes, all in the **cica** repo. The existing published worker image already works unchanged — its `ENTRYPOINT` is `["cica"]` (`Dockerfile:95`), so a Cloud Run `args` override of `["worker","--turn",<id>]` invokes `cica worker --turn <id>`. **No image change.**

**Out of scope (separate GCP-IaC track, agreed):** the IaC repo (sprout-equivalent, likely Terraform), the router GCE VM, GCS bucket / Secret Manager / Cloud Run Job resource provisioning + VPC/networking, and the live end-to-end deploy.

## SDK choice: official, all-typed

GCP now ships an **official Rust SDK** (`googleapis/google-cloud-rust`), GA-stable (1.x), covering both planes we need:

- **`google-cloud-storage`** (official) — object read/write + ADC. Data plane → `GcsStateStore`.
- **`google-cloud-run-v2`** v1.11.0 (official, "types and functions are stable") — `Jobs` client (run-job) + `Executions` client (get/cancel execution). Control plane → `CloudRunLauncher`.
- Both authenticate through the shared **`google-cloud-auth`** stack (ADC: `GOOGLE_APPLICATION_CREDENTIALS` → ADC file → metadata server → gcloud). MSRV 1.88 — cica is edition 2024 / toolchain 1.96, no MSRV pin, so it's a non-issue.

This is **symmetric with the AWS side** (`aws-sdk-s3`/`aws-sdk-ecs` are also official typed SDKs), so GCP mirrors AWS exactly: a typed client behind our own trait, lazy-init, fake for unit tests. We deliberately did **not** hand-roll REST — the earlier concern that Cloud Run's typed Rust coverage was thin is moot now that there's an official GA crate.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Dependencies | `google-cloud-storage` (feat `gcs`), `google-cloud-run-v2` (feat `cloudrun`), **optional** | All GCP deps (tonic/gRPC) stay behind features; the lean build is untouched. |
| Feature graph | `cloudrun = ["gcs", ...]`; `cloud = ["fargate", "cloudrun"]` | Cloud Run implies the GCS store, just as Fargate implies S3. `cloud` carrying both is what makes "one image serves AWS/GCP/local" literally true. |
| Traits | Reuse `StateStore` (`pull`/`push`) and `Launcher` (`launch(turn_id)`) unchanged | Only the store backend and the launch step are GCP-specific; `LaunchedWorkerProvider` + store-mediated job/result are unchanged. |
| GCP seam | A 3-method `CloudRunClient` trait (`run_job`/`get_execution`/`cancel_execution`); `google-cloud-run-v2` only in the real impl | The launch/poll/cancel state machine is unit-testable with a fake; the SDK stays at the edge. Same shape as `EcsClient`. |
| Auth | ADC via the SDK's `google-cloud-auth`, never config | Mirror of S3/ECS using the AWS chain. On GCP: the router VM's + the job's attached service accounts. |
| Run model | Poll `GetExecution` ourselves (don't block on the run-job LRO) | Keeps our own timeout + best-effort cancel; symmetric with the Fargate `DescribeTasks` poll. **(decided)** |
| `CloudRunConfig` networking | **No** subnet/SG/public-IP fields | Cloud Run networking (VPC connector, egress, ingress) is set on the **Job resource at deploy time** by the IaC; `RunJob` overrides are limited to args/env/task-count/timeout — there is no per-run network knob. The asymmetry vs. Fargate (which *must* pass `networkConfiguration` to `RunTask`) is the platform's shape, not a simplification. **(decided)** |
| Timeout handling | Best-effort `CancelExecution` on poll timeout, then bail | Never leak a running execution (cost + a late GCS result write). Failure to cancel is logged, not masked. Mirror of `StopTask`. |
| Build-without-feature | `store = "gcs"` / `provider = "cloudrun"` without the feature → fail fast | Same actionable error as the S3/Fargate arms. |
| Validation | Unit-tested + gated `fake-gcs-server` store test; real `RunJob` on the IaC track | Cloud Run has no local emulator (like Fargate); GCS does (`fake-gcs-server`, like LocalStack/MinIO for S3). |

## Config (`src/config.rs`)

```toml
[deployment]
provider = "cloudrun"
store = "gcs"          # required in practice

[deployment.gcs]
bucket = "cica-state-acme"      # required
prefix = "cica"                 # optional key namespace
# endpoint = "http://localhost:4443"   # optional; fake-gcs-server / testing only

[deployment.cloudrun]
project = "acme-prod"           # required
region  = "europe-west1"        # required (Cloud Run is regional)
job     = "cica-worker"         # required (the Cloud Run Job name)
# container_name = "cica-worker"      # optional; selects which container's args to override
poll_interval_secs = 5          # default 5
timeout_secs = 900              # default 900
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcsConfig {
    pub bucket: String,                 // required
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,       // fake-gcs-server / testing
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudRunConfig {
    pub project: String,                // required
    pub region: String,                 // required (regional service)
    pub job: String,                    // required (Cloud Run Job name)
    #[serde(default)]
    pub container_name: Option<String>, // None = the job's single container
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}
```

- `StoreKind::Gcs` (serde `"gcs"`) alongside `Filesystem`/`S3`; `ProviderKind::CloudRun` (serde `"cloudrun"`) alongside the rest.
- `DeploymentConfig` gains `#[serde(default)] pub gcs: Option<GcsConfig>` and `pub cloudrun: Option<CloudRunConfig>` (both always compile; only the impls are feature-gated, mirroring `S3Config`/`FargateConfig`).
- `GcsConfig` has **no region** — GCS object operations are addressed globally (the bucket carries its own location). This is a deliberate divergence from `S3Config`.
- Reuses the existing `default_poll_interval_secs` (5) / `default_timeout_secs` (900) fns.

## Components

### `GcsStateStore` (`src/sandbox/state/gcs.rs`, `#[cfg(feature = "gcs")]`)

Implements `StateStore` over GCS objects keyed `<prefix>/<key>/<relative-file-path>` — the same key scheme as `S3StateStore`, so the `object_key` / `dir_prefix` helpers and their tests carry over.

- **`pull(key, dest)`**: list objects under `dir_prefix(prefix, key)`; if none → `Ok(false)` (absent, matches the filesystem store); else `clear_dir(dest)` and download each object to `dest/<rel>`, guarding `..` in the relative path (parity with the S3 traversal guard).
- **`push(src, key)`**: replace semantics — delete everything currently under the prefix, then upload every file under `src`. **Large-file reliability is the crate's job**: the official `google-cloud-storage` client handles resumable uploads, so we do *not* re-derive the manual multipart/timeout logic that `S3StateStore` needed (that pain came from a single fragile `put_object`; GCS resumable uploads cover it).
- Lazy client via `OnceCell` so `default_store` stays sync (mirror of `S3StateStore`); ADC + optional `endpoint` override for `fake-gcs-server`.

### `CloudRunClient` trait + data types (`src/sandbox/cloudrun.rs`)

```rust
/// What CloudRunLauncher needs from Cloud Run — the seam keeping google-cloud-run-v2 at the edge.
#[async_trait]
trait CloudRunClient: Send + Sync {
    /// Start a job execution; returns the execution resource name. Errors on RunJob failure.
    async fn run_job(&self, req: &RunJobRequest) -> Result<String>;
    /// Current status of an execution (selected by resource name).
    async fn get_execution(&self, execution: &str) -> Result<ExecutionStatus>;
    /// Best-effort cancel (on timeout). Errors are logged by the caller, not fatal.
    async fn cancel_execution(&self, execution: &str) -> Result<()>;
}

struct RunJobRequest {
    project: String,
    region: String,
    job: String,
    container_name: Option<String>,   // ContainerOverride.name; None = sole container
    args: Vec<String>,                // ["worker", "--turn", <id>]
}

struct ExecutionStatus {
    terminal: bool,        // execution has completed (success or failure)
    succeeded: bool,       // succeededCount >= 1 / "Completed" condition true
    reason: Option<String>,// failure message when !succeeded
}
```

- `run_job` issues Cloud Run `RunJob` with an `overrides.containerOverrides[{name, args}]` payload and returns the **execution resource name** (`projects/{p}/locations/{region}/jobs/{job}/executions/{exec}`) — the analog of the Fargate task ARN. We extract the execution name (not block on the long-running operation) so we own the poll loop.
- `get_execution` maps the Execution's completion + `succeededCount`/`failedCount`/conditions → `ExecutionStatus`.
- `GcpRunClient` (real impl): lazy `OnceCell` over the typed `Jobs`/`Executions` clients, ADC auth, region carried in the resource path.

### `CloudRunLauncher` (`#[cfg(feature = "cloudrun")]`)

```rust
pub struct CloudRunLauncher {
    run: Box<dyn CloudRunClient>,
    config: CloudRunConfig,
}

#[async_trait]
impl Launcher for CloudRunLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let exec = self.run.run_job(&self.run_job_request(turn_id)).await?;
        let deadline = now + timeout;
        loop {
            let st = self.run.get_execution(&exec).await?;
            if st.terminal {
                return if st.succeeded { Ok(()) }
                       else { bail!("worker execution failed (reason {:?})", st.reason) };
            }
            if past_deadline {
                if let Err(e) = self.run.cancel_execution(&exec).await {
                    tracing::warn!("failed to cancel timed-out execution {exec}: {e}");
                }
                bail!("worker execution timed out after {timeout_secs}s");
            }
            sleep(poll_interval).await;
        }
    }
}
```

This is the `FargateLauncher::launch` loop with ECS terms swapped for Cloud Run terms. `run_job_request(turn_id)` is pure and unit-tested. `now`/`sleep` use `tokio::time`.

### Wiring

- **`state/mod.rs`**: `#[cfg(feature = "gcs")] pub mod gcs;` + a `StoreKind::Gcs` arm in `default_store` (requires `[deployment.gcs]`, lazy client keeps it sync, `#[cfg(not(feature="gcs"))]` → "build with `--features gcs`"). Mirror of the S3 arm.
- **`sandbox/mod.rs`**: `#[cfg(feature = "cloudrun")] mod cloudrun;` + a `ProviderKind::CloudRun` arm in `try_default_provider` (requires a store + `[deployment.cloudrun]`, builds `LaunchedWorkerProvider::new(store, Box::new(CloudRunLauncher::new(cc)))`, `#[cfg(not(feature="cloudrun"))]` → fail fast). Mirror of the Fargate arm. In practice the store is GCS (a filesystem store isn't reachable from a Cloud Run task) — documented, not code-enforced, as with Fargate/S3.
- **`prep_skill_deps_locally`**: extend `!matches!(provider, Some(Fargate))` → `!matches!(provider, Some(Fargate | CloudRun))`. CloudRun is a remote worker that hydrates its own skills copy, so installing skill deps on the router is wasted — identical rationale to Fargate.

## Env overlay (`overlay_from_env`, `src/config.rs`)

For the env-driven published-image **worker** (which runs from env alone):

- `CICA_STORE=gcs` → `StoreKind::Gcs` (add arm next to `s3`/`filesystem`).
- `CICA_GCS_BUCKET` → `gcs.bucket` (via `get_or_insert_with(Default::default)`).

That is all the worker needs. The `[deployment.cloudrun]` block (project/region/job) is **router-only** — the worker never launches jobs — and lives in the router's `config.toml`, not the env overlay, exactly as `FargateConfig` does. AI-key overlay (`CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY`) is unchanged and reused.

## Cargo features (`Cargo.toml`)

```toml
gcs      = ["dep:google-cloud-storage"]               # google-cloud-auth pulled transitively
cloudrun = ["gcs", "dep:google-cloud-run-v2"]
cloud    = ["fargate", "cloudrun"]                    # one published image carries AWS + GCP
```

(If `google-cloud-auth` turns out to need explicit listing during impl, add it; expectation is it's transitive.)

## Data flow (one turn, `provider = "cloudrun"`)

```
router → LaunchedWorkerProvider.run_turn(job)
  push turns/<id>/job → GcsStateStore
  CloudRunLauncher.launch(<id>):
    RunJob(project, region, job, overrides: container <name?> args ["worker","--turn",<id>]) → execution name
    poll GetExecution until terminal (or timeout → CancelExecution + bail)
      [Cloud Run task] XDG_CONFIG_HOME=/data → base=/data/cica
        cica worker: pull job ← GCS ; hydrate session+memories ← GCS into fresh homes
          run backend (cursor/claude; creds from CICA_*_API_KEY env)
          capture session + push, push memories → GCS
        push turns/<id>/result → GCS ; exit 0
    execution succeeded → Ok
  pull turns/<id>/result ← GCS → TurnResult ; cleanup turns/<id>
→ router posts response
```

## Error handling

- **RunJob failure** (bad job ref, quota, image pull) → `launch` returns `Err` → turn error; blobs cleaned up by `LaunchedWorkerProvider`.
- **Poll timeout** → best-effort `CancelExecution` (warn on failure), then `Err`.
- **Execution failed / non-zero task** → `Err` (no `result` blob was written).
- **GetExecution transient error** → propagate as `Err` (v1: no internal retry; the channel/cron retry path applies). Revisit if flaky.
- **Missing `[deployment.gcs]` / `[deployment.cloudrun]`** with the matching kind → clear config error.
- **Kind set without the feature** → fail fast at construction.

## Testing strategy

- **Unit (no GCP):**
  - `GcsStateStore` key helpers — reuse the `object_key` / `dir_prefix` / `strip_prefix` tests from S3.
  - `run_job_request(turn_id)` — asserts project/region/job/container_name and `args == ["worker","--turn",<id>]`.
  - `CloudRunLauncher` poll state machine with a scripted `FakeCloudRunClient`: terminal+succeeded → `Ok`; terminal+failed → `Err`; never-terminal → timeout `Err` **and** `cancel_execution` was called (fake records it). Mirror of the `FakeEcs` tests.
  - `default_store` / `try_default_provider` arms: feature-off → error; feature-on + section present → built lazily (no network). Mirror of the S3/Fargate config tests.
  - Env overlay: `CICA_STORE=gcs` + `CICA_GCS_BUCKET` → `StoreKind::Gcs` + bucket set.
- **Integration (gated, real GCS — NOT an emulator):** `GcsStateStore` round-trip (absent→false, push/pull byte-for-byte, replace semantics, a >resumable-threshold large file), gated by `CICA_GCS_IT` (+ `CICA_GCS_ENDPOINT`/`CICA_GCS_BUCKET`). The IT code is written and auto-skips when the env is unset.
  - **Emulator finding (2026-06-12, during impl):** the planned `fake-gcs-server` emulator is **infeasible** — the official `google-cloud-storage` SDK serves list/delete through `StorageControl`, a **gRPC-only** client with no REST toggle, while `fake-gcs-server` speaks JSON/REST only (verified: `HTTP2 GoAway / FRAME_SIZE_ERROR` on the first `list_objects`). Google's own `storage-testbench` does support gRPC but requires a two-step start (`/start_grpc`) and has known gRPC-in-Docker flakiness (testbench issue #295), so it was rejected for CI.
  - **Decision:** the live GCS IT is therefore **deferred to the GCP-IaC/deploy track against a real bucket** (real ADC, no `endpoint` override) — exactly the pattern used for Fargate's first real `RunTask`. There is **no GCS emulator job in CI.** The `endpoint`→anonymous-credentials affordance in `GcsStateStore` is retained for manual runs against a future gRPC-capable emulator or a real bucket via a custom endpoint.
- **Real `RunJob` deferred to the GCP-IaC track:** Cloud Run has no local emulator, so there is **no** CI service-container test for the launcher; the first real execution is the acceptance test once the IaC stands up the Job (mirrors Fargate→sprout).
- **CI build/lint/unit lane:** `cargo clippy --features cloud --all-targets -- -D warnings` + `cargo test --features cloud` so both `gcs` and `cloudrun` code compiles, lints, and its unit tests run (the gated IT auto-skips). This replaces the planned standalone `gcs-store` emulator job — there is no emulator job.

## Distribution impact

- Default `cargo build` + `install.sh` unchanged — the GCP crates are optional, pulled only by `--features gcs`/`cloudrun`/`cloud`.
- The env overlay additions are plain config logic (no dependency) and always compiled, so local/Docker get them for free.
- Release: `release.yml`'s cloud variants build with `--features cloud` (now = AWS + GCP); the published `cica-worker` image is unchanged (the launcher is router-side, and the image already entrypoints `cica`). One image serves AWS, GCP, and local.

## Out of scope (later)

- The GCP IaC repo (Cloud Run Job, GCS bucket, Secret Manager, service accounts, VPC/networking, router GCE VM) + the first real `RunJob` end-to-end.
- Warm/reused executions, internal `GetExecution` retry/backoff tuning, `CancelExecution` on non-timeout error paths, Cloud Logging wiring → revisit if needed.
