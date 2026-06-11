# Phase 3b-3: GCP Worker Fleet Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add feature-gated `GcsStateStore` (Google Cloud Storage) and `CloudRunLauncher` (Cloud Run Jobs) so the cica worker fleet runs on GCP, mirroring the existing AWS `S3StateStore`/`FargateLauncher` pair behind the unchanged `StateStore`/`Launcher` traits.

**Architecture:** Two new modules — `src/sandbox/state/gcs.rs` (feature `gcs`) and `src/sandbox/cloudrun.rs` (feature `cloudrun`) — each wrapping an official typed Google SDK behind a small internal seam (the `StateStore` trait for the store; a new `CloudRunClient` trait for the launcher) so the state machines are unit-testable with fakes and the SDK stays at the edge. Config gains `StoreKind::Gcs`/`ProviderKind::CloudRun` + `GcsConfig`/`CloudRunConfig`; `cloud = ["fargate", "cloudrun"]` makes one published image serve both clouds. No worker-image change (entrypoint is already `cica`).

**Tech Stack:** Rust (edition 2024), `google-cloud-storage` + `google-cloud-run-v2` (official `googleapis/google-cloud-rust`, ADC auth via `google-cloud-auth`), `async-trait`, `tokio`, `fake-gcs-server` (Docker) for the gated store integration test.

**Spec:** `docs/superpowers/specs/2026-06-09-phase3b3-gcp-worker-fleet-design.md`

**Reference (mirror these closely):** `src/sandbox/state/s3.rs`, `src/sandbox/fargate.rs`, `src/config.rs` (`S3Config`/`FargateConfig`/`overlay_from_env`), `src/sandbox/mod.rs` (`try_default_provider`).

---

## Task 1: SDK spike — pin versions and capture the exact API surface

The two official crates' exact method names/signatures are not assumed by this plan; this task pins them so Tasks 4 and 7 can use real calls instead of guesses. Output is a short reference note the later tasks consume. **No production code is written in this task.**

**Files:**
- Create: `docs/superpowers/plans/2026-06-09-phase3b3-sdk-notes.md`

- [ ] **Step 1: Pin crate versions**

Run: `cargo search google-cloud-storage` and `cargo search google-cloud-run-v2`
Record the latest released versions of `google-cloud-storage` and `google-cloud-run-v2` (and whether `google-cloud-auth` must be listed explicitly or is transitive).

- [ ] **Step 2: Capture the GCS object-IO surface**

In a scratch crate or via docs.rs for the pinned `google-cloud-storage`, record the exact calls for: building a client with ADC + an `endpoint` override (for `fake-gcs-server`); listing objects under a prefix (with pagination); downloading an object's bytes; uploading a file (resumable/large-file path). Write each as a 1-3 line snippet into the notes file under a `## GCS` heading.

- [ ] **Step 3: Capture the Cloud Run surface**

For the pinned `google-cloud-run-v2`, record the exact calls for: building the `Jobs` and `Executions` clients with ADC; `RunJob` with `overrides.container_overrides[{ name, args }]` and how to read the returned **execution resource name** without awaiting the long-running operation; `GetExecution` and which fields signal terminal/succeeded/failed (e.g. completion time, `succeeded_count`/`failed_count`, conditions); `CancelExecution`. Write these under a `## Cloud Run` heading.

- [ ] **Step 4: Commit the notes**

```bash
git add docs/superpowers/plans/2026-06-09-phase3b3-sdk-notes.md
git commit -m "docs(phase3b3): SDK API notes (gcs + cloud-run-v2)"
```

---

## Task 2: Config — `StoreKind::Gcs`, `ProviderKind::CloudRun`, config structs

Pure config additions. Compiles with no new dependencies (structs always compile; only impls are feature-gated, exactly like `S3Config`/`FargateConfig`).

**Files:**
- Modify: `src/config.rs` (enums near lines 118-133; structs after `FargateConfig` ~line 202; `DeploymentConfig` ~lines 205-225)
- Test: `src/config.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/config.rs`:

```rust
#[test]
fn parses_gcs_store_and_cloudrun_provider() {
    let toml = r#"
        [deployment]
        provider = "cloudrun"
        store = "gcs"

        [deployment.gcs]
        bucket = "cica-state-acme"
        prefix = "cica"

        [deployment.cloudrun]
        project = "acme-prod"
        region = "europe-west1"
        job = "cica-worker"
    "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.deployment.store, Some(StoreKind::Gcs));
    assert_eq!(cfg.deployment.provider, Some(ProviderKind::CloudRun));
    let gcs = cfg.deployment.gcs.unwrap();
    assert_eq!(gcs.bucket, "cica-state-acme");
    assert_eq!(gcs.prefix.as_deref(), Some("cica"));
    let cr = cfg.deployment.cloudrun.unwrap();
    assert_eq!(cr.project, "acme-prod");
    assert_eq!(cr.region, "europe-west1");
    assert_eq!(cr.job, "cica-worker");
    assert_eq!(cr.poll_interval_secs, 5); // serde default
    assert_eq!(cr.timeout_secs, 900); // serde default
    assert!(cr.container_name.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib parses_gcs_store_and_cloudrun_provider`
Expected: FAIL — `no variant Gcs` / `no field gcs`.

- [ ] **Step 3: Add the enum variants**

In `src/config.rs`, extend the two enums:

```rust
pub enum StoreKind {
    Filesystem,
    S3,
    Gcs,
}
```
```rust
pub enum ProviderKind {
    Local,
    Subprocess,
    Docker,
    Fargate,
    CloudRun,
}
```
(Both already carry `#[serde(rename_all = "lowercase")]`, so they parse from `"gcs"` / `"cloudrun"`.)

- [ ] **Step 4: Add the config structs**

After `FargateConfig` in `src/config.rs`:

```rust
/// GCS state-store settings (used when `store = "gcs"`). Credentials come from
/// Application Default Credentials (ADC), never config. No region: GCS object
/// operations are addressed globally (the bucket carries its own location).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcsConfig {
    /// Bucket name (required).
    pub bucket: String,
    /// Optional key namespace within the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Optional endpoint override (fake-gcs-server / testing).
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Cloud Run launcher settings (used when `provider = "cloudrun"`). Credentials
/// come from ADC (the router VM's service account), never config. Networking
/// (VPC connector, egress) is configured on the Cloud Run Job resource by the
/// IaC — `RunJob` has no per-run network knob — so there are no network fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudRunConfig {
    /// GCP project id (required).
    pub project: String,
    /// Region, e.g. "europe-west1" (required — Cloud Run is regional).
    pub region: String,
    /// Cloud Run Job name to execute (required).
    pub job: String,
    /// Which container's args to override; `None` = the job's single container.
    #[serde(default)]
    pub container_name: Option<String>,
    /// GetExecution poll interval in seconds.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Max seconds to wait for the execution to finish before bailing.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}
```
(Reuses the existing `default_poll_interval_secs` / `default_timeout_secs` fns.)

- [ ] **Step 5: Wire into `DeploymentConfig`**

Add two fields to `DeploymentConfig`:

```rust
    /// GCS store settings (used when `store = "gcs"`).
    #[serde(default)]
    pub gcs: Option<GcsConfig>,
    /// Cloud Run launcher settings (used when `provider = "cloudrun"`).
    #[serde(default)]
    pub cloudrun: Option<CloudRunConfig>,
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test --lib parses_gcs_store_and_cloudrun_provider`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): GcsConfig + CloudRunConfig + Gcs/CloudRun kinds"
```

---

## Task 3: Env overlay + `prep_skill_deps_locally`

The worker reads its store config from env (published-image path). Add `CICA_STORE=gcs` and `CICA_GCS_BUCKET`. Also mark CloudRun a remote provider so the router skips local skill-dep installs.

**Files:**
- Modify: `src/config.rs` — `overlay_from_env` (~lines 459-492), `prep_skill_deps_locally` (~lines 135-140)
- Test: `src/config.rs` (`tests` module)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn env_overlay_sets_gcs_store_and_bucket() {
    let mut cfg = Config::default();
    let env = |k: &str| match k {
        "CICA_STORE" => Some("gcs".to_string()),
        "CICA_GCS_BUCKET" => Some("cica-state-acme".to_string()),
        _ => None,
    };
    cfg.overlay_from_env(env);
    assert_eq!(cfg.deployment.store, Some(StoreKind::Gcs));
    assert_eq!(cfg.deployment.gcs.unwrap().bucket, "cica-state-acme");
}

#[test]
fn cloudrun_skips_local_skill_deps() {
    assert!(!prep_skill_deps_locally(Some(ProviderKind::CloudRun)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib env_overlay_sets_gcs_store_and_bucket cloudrun_skips_local_skill_deps`
Expected: FAIL — `gcs` arm missing / `CloudRun` not matched.

- [ ] **Step 3: Extend the `CICA_STORE` match and add the bucket overlay**

In `overlay_from_env`, add the `"gcs"` arm to the existing `CICA_STORE` match:

```rust
        if let Some(v) = get("CICA_STORE") {
            match v.as_str() {
                "s3" => self.deployment.store = Some(StoreKind::S3),
                "gcs" => self.deployment.store = Some(StoreKind::Gcs),
                "filesystem" => self.deployment.store = Some(StoreKind::Filesystem),
                other => tracing::warn!("ignoring unknown CICA_STORE={other}"),
            }
        }
```

After the `CICA_S3_REGION` block, add:

```rust
        if let Some(v) = get("CICA_GCS_BUCKET") {
            self.deployment
                .gcs
                .get_or_insert_with(Default::default)
                .bucket = v;
        }
```

- [ ] **Step 4: Extend `prep_skill_deps_locally`**

```rust
pub fn prep_skill_deps_locally(provider: Option<ProviderKind>) -> bool {
    !matches!(
        provider,
        Some(ProviderKind::Fargate) | Some(ProviderKind::CloudRun)
    )
}
```
Update its doc comment to read `Only false for Fargate/Cloud Run, where turns run on a remote worker...`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib env_overlay_sets_gcs_store_and_bucket cloudrun_skips_local_skill_deps`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): CICA_STORE=gcs / CICA_GCS_BUCKET overlay + CloudRun remote"
```

---

## Task 4: `GcsStateStore` (feature `gcs`)

Mirror `S3StateStore`: same `<prefix>/<key>/<rel>` key scheme (reuse the helper logic), `pull`/`push` with replace semantics, lazy client, `endpoint` override for the emulator. The SDK object-IO calls come from the Task 1 notes; everything else is fixed here.

**Files:**
- Create: `src/sandbox/state/gcs.rs`
- Modify: `src/sandbox/state/mod.rs` (module decl ~lines 6-10; `default_store` ~lines 44-64)
- Modify: `Cargo.toml` (`[dependencies]` ~line 82; `[features]` ~line 89)
- Test: `src/sandbox/state/gcs.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Add the dependency and feature**

In `Cargo.toml` `[dependencies]` (next to the aws optional deps), pin the version from Task 1:

```toml
google-cloud-storage = { version = "<pinned>", optional = true }
```
In `[features]`:
```toml
gcs = ["dep:google-cloud-storage"]
```

- [ ] **Step 2: Write the failing unit tests (key helpers)**

Create `src/sandbox/state/gcs.rs` with the same pure key helpers as S3 and their tests (copy `object_key`/`dir_prefix` from `s3.rs:345-364` and the `tests` module from `s3.rs:453-482`, renaming nothing — they are identical). This guarantees the GCS key scheme matches S3 byte-for-byte.

- [ ] **Step 3: Run to verify the helper tests fail (module not wired)**

Run: `cargo test --features gcs --lib sandbox::state::gcs`
Expected: FAIL to compile — `gcs` module not declared yet.

- [ ] **Step 4: Declare the module**

In `src/sandbox/state/mod.rs`, after the S3 decl:
```rust
#[cfg(feature = "gcs")]
pub mod gcs;
```

- [ ] **Step 5: Implement `GcsStateStore`**

In `src/sandbox/state/gcs.rs`, add the struct and `StateStore` impl, mirroring `S3StateStore` (`s3.rs:20-205`). Structure (fill the SDK calls from the Task 1 `## GCS` notes):

```rust
use std::fs;
use std::path::Path;
use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::config::GcsConfig;
use crate::sandbox::state::{StateStore, clear_dir};

pub struct GcsStateStore {
    config: GcsConfig,
    prefix: String, // normalized: no leading/trailing slashes
    client: OnceCell</* google-cloud-storage client type from notes */>,
}

impl GcsStateStore {
    pub fn new(config: GcsConfig) -> Self {
        let prefix = config.prefix.clone().unwrap_or_default().trim_matches('/').to_string();
        Self { config, prefix, client: OnceCell::new() }
    }
    async fn client(&self) -> Result<&/* client type */> {
        self.client.get_or_try_init(|| async { build_client(&self.config).await }).await
    }
}

// build_client: ADC by default; apply config.endpoint when set (fake-gcs-server). From notes.

#[async_trait]
impl StateStore for GcsStateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        // 1. list objects under dir_prefix(&self.prefix, key) (paginate per notes)
        // 2. if none -> Ok(false)   (absent, matches FilesystemStateStore)
        // 3. clear_dir(dest)
        // 4. for each object: rel = key.strip_prefix(&prefix); reject any ".." segment
        //    (parity with s3.rs:117-119); download bytes (notes); fs::write(dest.join(rel))
        // 5. Ok(true)
    }
    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        // 1. delete every object under dir_prefix (replace semantics, paginate)
        // 2. for each file under src (reuse walk_files from s3.rs:326-340):
        //    rel = path.strip_prefix(src) with '\\' -> '/'; obj = object_key(&prefix, key, &rel);
        //    upload the file (resumable per notes — no manual multipart needed, the crate handles it)
        // 3. Ok(())
    }
}
```
Copy `walk_files`, `object_key`, `dir_prefix` verbatim from `s3.rs`. Keep the `..` guard in `pull` (`s3.rs:117-119`).

- [ ] **Step 6: Add the `default_store` arm**

In `src/sandbox/state/mod.rs` `default_store`, after the `S3` arm:

```rust
        Some(StoreKind::Gcs) => {
            #[cfg(feature = "gcs")]
            {
                let gcs = config.deployment.gcs.clone().ok_or_else(|| {
                    anyhow::anyhow!("`store = gcs` requires a [deployment.gcs] section")
                })?;
                Ok(Some(Arc::new(gcs::GcsStateStore::new(gcs))))
            }
            #[cfg(not(feature = "gcs"))]
            {
                anyhow::bail!("`store = gcs` requires the binary to be built with `--features gcs`")
            }
        }
```

- [ ] **Step 7: Add `default_store` config tests**

In the `state::mod` `tests`, mirror the S3 tests (`mod.rs:171-200`):

```rust
#[cfg(not(feature = "gcs"))]
#[test]
fn gcs_store_requires_feature() {
    let mut cfg = Config::default();
    cfg.deployment.store = Some(StoreKind::Gcs);
    assert!(default_store(&cfg).is_err());
}

#[cfg(feature = "gcs")]
#[test]
fn gcs_store_built_lazily_when_feature_on() {
    use crate::config::GcsConfig;
    let mut cfg = Config::default();
    cfg.deployment.store = Some(StoreKind::Gcs);
    cfg.deployment.gcs = Some(GcsConfig { bucket: "b".into(), ..Default::default() });
    assert!(default_store(&cfg).unwrap().is_some());
}

#[cfg(feature = "gcs")]
#[test]
fn gcs_store_without_section_errors() {
    let mut cfg = Config::default();
    cfg.deployment.store = Some(StoreKind::Gcs);
    cfg.deployment.gcs = None;
    assert!(default_store(&cfg).is_err());
}
```

- [ ] **Step 8: Add the gated integration test**

In `src/sandbox/state/gcs.rs`, add an `it_tests` module mirroring `s3.rs:366-451`, gated on `CICA_GCS_IT`, pointing `endpoint` at `CICA_GCS_ENDPOINT` (default `http://localhost:4443`) and bucket at `CICA_GCS_BUCKET` (default `cica-test`). Cover: absent→`false`; push/pull a nested tree byte-for-byte; push replaces prior contents; a >resumable-threshold large file round-trips.

- [ ] **Step 9: Run all GCS tests (unit; IT skipped without the env)**

Run: `cargo test --features gcs --lib sandbox::state::gcs`
Expected: PASS (the `it_tests` early-return without `CICA_GCS_IT`).

- [ ] **Step 10: Run the gated IT against fake-gcs-server**

```bash
docker run -d --name fake-gcs -p 4443:4443 fsouza/fake-gcs-server -scheme http -port 4443
CICA_GCS_IT=1 CICA_GCS_ENDPOINT=http://localhost:4443 CICA_GCS_BUCKET=cica-test \
  cargo test --features gcs --lib sandbox::state::gcs::it_tests -- --include-ignored
docker rm -f fake-gcs
```
Expected: PASS (round-trip + replace + large file). If the emulator needs the bucket pre-created, create it in the test setup or via the emulator's bootstrap flag.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/sandbox/state/
git commit -m "feat(state): GcsStateStore (feature gcs) + gated fake-gcs-server IT"
```

---

## Task 5: CI `gcs-store` job

Mirror the `s3-store` CI job, swapping LocalStack for `fake-gcs-server`.

**Files:**
- Modify: the CI workflow that defines `s3-store` (find with `grep -rl 's3-store' .github/workflows`)

- [ ] **Step 1: Add the job**

Copy the `s3-store` job to a `gcs-store` job: a `fsouza/fake-gcs-server` service container (or `docker run` step) on `4443` with `-scheme http`, then:
```bash
CICA_GCS_IT=1 CICA_GCS_ENDPOINT=http://localhost:4443 CICA_GCS_BUCKET=cica-test \
  cargo test --features gcs --lib sandbox::state::gcs -- --include-ignored
```
Match the existing job's runner, cache, and checkout steps exactly.

- [ ] **Step 2: Verify the workflow parses**

Run: `grep -n 'gcs-store' .github/workflows/*.yml` and confirm the YAML indentation matches the sibling job.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/
git commit -m "ci: gcs-store job (fake-gcs-server integration test)"
```

---

## Task 6: `CloudRunClient` trait + `CloudRunLauncher` + fake (feature `cloudrun`)

The SDK-agnostic half: the trait seam, pure request builder, the launch/poll/cancel state machine, and the full fake-driven unit tests — a direct mirror of `fargate.rs`. The real SDK client is Task 7.

**Files:**
- Create: `src/sandbox/cloudrun.rs`
- Modify: `src/sandbox/mod.rs` (module decl ~lines 7-13)
- Modify: `Cargo.toml` (`[dependencies]`; `[features]`)
- Test: `src/sandbox/cloudrun.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Add the dependency and feature**

In `Cargo.toml` `[dependencies]`, pin from Task 1:
```toml
google-cloud-run-v2 = { version = "<pinned>", optional = true }
```
In `[features]`:
```toml
cloudrun = ["gcs", "dep:google-cloud-run-v2"]
```

- [ ] **Step 2: Declare the module**

In `src/sandbox/mod.rs`, after the fargate decl:
```rust
#[cfg(feature = "cloudrun")]
mod cloudrun;
```

- [ ] **Step 3: Write the failing tests**

In `src/sandbox/cloudrun.rs`, add the `tests` module mirroring `fargate.rs:253-383`:

```rust
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
            Self { statuses: Mutex::new(statuses.into()), run_ok: true, cancel_called: AtomicBool::new(false) }
        }
        fn failing_run() -> Self {
            Self { statuses: Mutex::new(VecDeque::new()), run_ok: false, cancel_called: AtomicBool::new(false) }
        }
    }
    #[async_trait]
    impl CloudRunClient for FakeRun {
        async fn run_job(&self, _req: &RunJobRequest) -> Result<String> {
            if self.run_ok { Ok("projects/acme/locations/europe-west1/jobs/cica-worker/executions/e1".into()) }
            else { anyhow::bail!("run_job failed") }
        }
        async fn get_execution(&self, _e: &str) -> Result<ExecutionStatus> {
            let mut q = self.statuses.lock().unwrap();
            if q.len() > 1 { Ok(q.pop_front().unwrap()) } else { Ok(q.front().cloned().unwrap()) }
        }
        async fn cancel_execution(&self, _e: &str) -> Result<()> {
            self.cancel_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    fn st(terminal: bool, succeeded: bool) -> ExecutionStatus {
        ExecutionStatus { terminal, succeeded, reason: None }
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
        c.timeout_secs = 0; // first non-terminal poll is already past deadline
        let fake = std::sync::Arc::new(FakeRun::new(vec![st(false, false)]));
        struct ArcRun(std::sync::Arc<FakeRun>);
        #[async_trait]
        impl CloudRunClient for ArcRun {
            async fn run_job(&self, r: &RunJobRequest) -> Result<String> { self.0.run_job(r).await }
            async fn get_execution(&self, e: &str) -> Result<ExecutionStatus> { self.0.get_execution(e).await }
            async fn cancel_execution(&self, e: &str) -> Result<()> { self.0.cancel_execution(e).await }
        }
        let l = CloudRunLauncher::with_client(Box::new(ArcRun(fake.clone())), c);
        assert!(l.launch("t1").await.is_err());
        assert!(fake.cancel_called.load(Ordering::SeqCst));
    }
}
```

- [ ] **Step 4: Run to verify they fail**

Run: `cargo test --features cloudrun --lib sandbox::cloudrun`
Expected: FAIL to compile — types not defined.

- [ ] **Step 5: Implement the trait, data types, and launcher**

At the top of `src/sandbox/cloudrun.rs` (mirror of `fargate.rs:1-52,186-251`):

```rust
//! Cloud Run launcher (feature `cloudrun`).
//!
//! Implements `Launcher` via Cloud Run `RunJob` + a `GetExecution` poll, reusing
//! the store-mediated job/result protocol. Google Cloud calls sit behind the
//! `CloudRunClient` trait so the launch/poll/cancel state machine is testable
//! without GCP; `google-cloud-run-v2` lives only in `GcpRunClient` (Task 7).

use anyhow::{Result, bail};
use async_trait::async_trait;
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
    /// Start an execution; returns its resource name. Errors on RunJob failure.
    async fn run_job(&self, req: &RunJobRequest) -> Result<String>;
    /// Current status of an execution (by resource name).
    async fn get_execution(&self, execution: &str) -> Result<ExecutionStatus>;
    /// Best-effort cancel (on timeout). The caller logs errors; not fatal.
    async fn cancel_execution(&self, execution: &str) -> Result<()>;
}

pub struct CloudRunLauncher {
    run: Box<dyn CloudRunClient>,
    config: CloudRunConfig,
}

impl CloudRunLauncher {
    /// Build the GCP-backed launcher (lazy client). Implemented in Task 7.
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
                bail!("worker execution timed out after {}s", self.config.timeout_secs);
            }
            sleep(interval).await;
        }
    }
}
```

To keep this task's tests independent of Task 7, temporarily stub the real client at the bottom of the file so `CloudRunLauncher::new` compiles:
```rust
struct GcpRunClient;
impl GcpRunClient {
    fn new(_config: &CloudRunConfig) -> Self { GcpRunClient }
}
#[async_trait]
impl CloudRunClient for GcpRunClient {
    async fn run_job(&self, _req: &RunJobRequest) -> Result<String> { bail!("GcpRunClient not yet implemented (Task 7)") }
    async fn get_execution(&self, _e: &str) -> Result<ExecutionStatus> { bail!("GcpRunClient not yet implemented (Task 7)") }
    async fn cancel_execution(&self, _e: &str) -> Result<()> { bail!("GcpRunClient not yet implemented (Task 7)") }
}
```

- [ ] **Step 6: Run to verify tests pass**

Run: `cargo test --features cloudrun --lib sandbox::cloudrun`
Expected: PASS (all 5 tests; the stub client is never exercised by the fake-driven tests).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/sandbox/cloudrun.rs src/sandbox/mod.rs
git commit -m "feat(cloudrun): CloudRunClient seam + CloudRunLauncher state machine"
```

---

## Task 7: `GcpRunClient` — real `google-cloud-run-v2` impl

Replace the Task 6 stub with the real typed client. Not unit-testable without GCP (no emulator); it must compile + lint under `--features cloudrun`, and the real `RunJob` is validated on the IaC track.

**Files:**
- Modify: `src/sandbox/cloudrun.rs` (replace the stub `GcpRunClient`)

- [ ] **Step 1: Implement `GcpRunClient` from the Task 1 notes**

Replace the stub with a lazy-client real impl, mirroring `AwsEcsClient` (`fargate.rs:54-184`):

```rust
use tokio::sync::OnceCell;

pub(crate) struct GcpRunClient {
    config: CloudRunConfig,
    // lazily-built Jobs + Executions clients (types from the SDK notes)
    clients: OnceCell</* (Jobs, Executions) */>,
}

impl GcpRunClient {
    pub(crate) fn new(config: &CloudRunConfig) -> Self {
        Self { config: config.clone(), clients: OnceCell::new() }
    }
    async fn clients(&self) -> Result<&/* (Jobs, Executions) */> {
        self.clients.get_or_try_init(|| async {
            // build both clients with ADC (notes). Returns Ok((jobs, executions)).
        }).await
    }
}

#[async_trait]
impl CloudRunClient for GcpRunClient {
    async fn run_job(&self, req: &RunJobRequest) -> Result<String> {
        // Build the job resource name: projects/{project}/locations/{region}/jobs/{job}
        // Call RunJob with overrides.container_overrides = [{ name: req.container_name, args: req.args }]
        // Extract and return the execution resource name from the returned operation
        // (per notes — do NOT await the LRO to completion).
    }
    async fn get_execution(&self, execution: &str) -> Result<ExecutionStatus> {
        // GetExecution(name = execution). Map to ExecutionStatus:
        //   terminal  = execution has a completion time / terminal condition
        //   succeeded = succeeded_count >= task count (or "Completed" condition == True)
        //   reason    = failure message when !succeeded
    }
    async fn cancel_execution(&self, execution: &str) -> Result<()> {
        // CancelExecution(name = execution). Map errors with .context(...).
    }
}
```
Use the exact types/method names recorded in `2026-06-09-phase3b3-sdk-notes.md`. The job-name builder is a small pure helper — add a `fn job_name(project, region, job) -> String` and a unit test for it (`"projects/acme/locations/europe-west1/jobs/cica-worker"`).

- [ ] **Step 2: Compile and lint under the feature**

Run: `cargo clippy --features cloudrun --all-targets -- -D warnings`
Expected: PASS (no warnings). Fix any SDK-type mismatches against the notes.

- [ ] **Step 3: Run the unit tests (fake-driven, still green)**

Run: `cargo test --features cloudrun --lib sandbox::cloudrun`
Expected: PASS — the fake-driven tests are unaffected; the new `job_name` test passes.

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/cloudrun.rs
git commit -m "feat(cloudrun): GcpRunClient over google-cloud-run-v2 (ADC)"
```

---

## Task 8: Provider wiring — `ProviderKind::CloudRun` arm

Wire the launcher into `try_default_provider`, mirroring the Fargate arm.

**Files:**
- Modify: `src/sandbox/mod.rs` (`try_default_provider`, after the Fargate arm ~line 137; `tests` ~lines 205-220)

- [ ] **Step 1: Write the failing tests**

In the `sandbox::mod` `tests`, mirror the Fargate config tests:

```rust
#[cfg(not(feature = "cloudrun"))]
#[test]
fn cloudrun_provider_requires_feature() {
    use crate::config::{Config, ProviderKind, StoreKind};
    let mut cfg = Config::default();
    cfg.deployment.provider = Some(ProviderKind::CloudRun);
    cfg.deployment.store = Some(StoreKind::Filesystem);
    cfg.deployment.state_path = Some("/tmp/cica-cr-test".into());
    assert!(try_default_provider(&cfg).is_err());
}

#[test]
fn cloudrun_provider_requires_a_store() {
    use crate::config::{Config, ProviderKind};
    let mut cfg = Config::default();
    cfg.deployment.provider = Some(ProviderKind::CloudRun);
    assert!(try_default_provider(&cfg).is_err());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib sandbox::cloudrun_provider`
Expected: FAIL — `CloudRun` arm missing → non-exhaustive match won't compile (compile error counts as the failing state).

- [ ] **Step 3: Add the provider arm**

In `try_default_provider`, after the `ProviderKind::Fargate` arm:

```rust
        ProviderKind::CloudRun => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = cloudrun` requires [deployment].store to be set")
            })?;
            #[cfg(feature = "cloudrun")]
            {
                let cc = config.deployment.cloudrun.clone().ok_or_else(|| {
                    anyhow::anyhow!("`provider = cloudrun` requires a [deployment.cloudrun] section")
                })?;
                Ok(Box::new(worker::LaunchedWorkerProvider::new(
                    store,
                    Box::new(cloudrun::CloudRunLauncher::new(cc)),
                )))
            }
            #[cfg(not(feature = "cloudrun"))]
            {
                let _ = store;
                anyhow::bail!(
                    "`provider = cloudrun` requires the binary to be built with `--features cloudrun`"
                )
            }
        }
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib sandbox::cloudrun_provider` and `cargo test --features cloudrun --lib sandbox::cloudrun_provider`
Expected: PASS in both (feature-off hits the requires-feature test; feature-on hits the requires-a-store test).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/mod.rs
git commit -m "feat(sandbox): wire ProviderKind::CloudRun into try_default_provider"
```

---

## Task 9: `cloud` umbrella + CI clippy lane + release

Make `cloud` carry both clouds and ensure the GCP code builds/lints in CI and ships in the cloud release artifacts.

**Files:**
- Modify: `Cargo.toml` (`[features]`)
- Modify: CI workflow(s) — the clippy lane and `release.yml`

- [ ] **Step 1: Extend the `cloud` feature**

In `Cargo.toml`:
```toml
cloud = ["fargate", "cloudrun"]
```

- [ ] **Step 2: Verify the whole cloud surface builds + lints**

Run: `cargo clippy --features cloud --all-targets -- -D warnings`
Expected: PASS — both `gcs`/`cloudrun` and `s3`/`fargate` code compile together.

- [ ] **Step 3: Add/extend the CI clippy lane**

In the workflow that lints the AWS features, add (or widen an existing step to) `--features cloud` so both clouds' code is linted in CI:
```bash
cargo clippy --features cloud --all-targets -- -D warnings
```

- [ ] **Step 4: Confirm release builds the cloud variant with both clouds**

In `release.yml`, verify the cloud binary variant builds with `--features cloud` (now AWS+GCP). No image change — the published `cica-worker` image is router-agnostic and already entrypoints `cica`. If `release.yml` names features explicitly anywhere as `fargate`/`s3`, leave them; the `cloud` umbrella is what the cloud artifacts use.

- [ ] **Step 5: Full test sweep**

Run: `cargo test --all && cargo test --features cloud --lib`
Expected: PASS (default build unchanged; cloud features compile and their unit tests pass).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml .github/workflows/
git commit -m "feat(cloud): cloud = fargate + cloudrun; CI lints both clouds"
```

---

## Self-Review

**Spec coverage:**
- Module layout (gcs.rs, cloudrun.rs) → Tasks 4, 6/7. ✅
- Config (`Gcs`/`CloudRun` kinds, `GcsConfig`/`CloudRunConfig`, no-region/no-network) → Task 2. ✅
- `CloudRunClient` seam + poll/cancel state machine → Task 6. ✅
- `GcpRunClient` real impl → Task 7. ✅
- `GcsStateStore` (key scheme reuse, replace semantics, resumable upload, `..` guard) → Task 4. ✅
- Selection wiring (`default_store` Gcs arm, `try_default_provider` CloudRun arm, `prep_skill_deps_locally`) → Tasks 4, 8, 3. ✅
- Env overlay (`CICA_STORE=gcs`, `CICA_GCS_BUCKET`) → Task 3. ✅
- Features (`gcs`, `cloudrun`, `cloud`) → Tasks 4, 6, 9. ✅
- Tests (unit + gated `fake-gcs-server` IT + CI lanes; real `RunJob` deferred) → Tasks 4, 5, 6, 9. ✅
- No image change (entrypoint already `cica`) → noted in Task 9. ✅
- SDK uncertainty handled honestly → Task 1 spike feeds Tasks 4 & 7. ✅

**Type consistency:** `RunJobRequest`{project,region,job,container_name,args}, `ExecutionStatus`{terminal,succeeded,reason}, and `CloudRunClient`{run_job,get_execution,cancel_execution} are used identically in Tasks 6, 7, and the tests. `GcsStateStore::new(GcsConfig)` / `CloudRunLauncher::new(CloudRunConfig)` match the `default_store` / `try_default_provider` call sites. Feature names (`gcs`, `cloudrun`, `cloud`) consistent throughout.

**Placeholder note:** Tasks 4 and 7 intentionally leave the *SDK call bodies* to be filled from Task 1's pinned notes — this is a deliberate spike-first dependency, not a vague placeholder; the surrounding code, behavioral contract, and tests are fully specified.
