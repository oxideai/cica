# Phase 3b-2a: `S3StateStore` (feature-gated)

**Date:** 2026-06-04
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 2 (`StateStore` trait + `FilesystemStateStore`), Phase 3b-1 (containerized worker + `Launcher` trait).

## Goal

Add an S3-backed `StateStore` so the router and ephemeral cloud workers can share durable state (sessions, memories, the turn job/result blobs) without a shared filesystem — the prerequisite for the AWS Fargate launcher (3b-2b) and the `sprout` CDK (3b-2c). Feature-gated so the default build stays lean; testable locally against LocalStack with no real AWS.

## Where this fits (Phase 3b-2 decomposition)

- **3b-2a (this spec):** `S3StateStore` (feature `s3`) — the cloud-portable store.
- **3b-2b:** `FargateLauncher` (`ecs:RunTask` + `DescribeTasks` poll) + the cloud worker config/secrets contract; reuses the store + the fake-backend test harness.
- **3b-2c:** the `sprout` CDK (ECR + push, S3 bucket, task-def, IAM, networking, secrets, wiring the existing router box).

Cross-cutting decisions already settled: the router keeps running on the existing EC2 box (sprout adds the worker fleet + S3 + the router's IAM role around it); cloud workers get config/creds via env from Secrets Manager (3b-2b); AWS code is tested against LocalStack.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Dependency | `aws-sdk-s3` + `aws-config`, **optional**, behind `[features] s3` | Default build stays lean (no AWS SDK); cloud builds opt in. |
| Trait | Implement the existing `StateStore` (`pull`/`push`) unchanged | Nothing else in the system changes; router/worker are store-agnostic. |
| Semantics | Mirror `FilesystemStateStore`: `pull → false` if absent; `push` **replaces** prior contents | Identical behavior across stores so sessions/memory/turns round-trip the same way. |
| Object layout | `<prefix>/<key>/<relative-file-path>` per file | The `StateStore` is dir-tree oriented; S3 has no dirs, so flatten to object keys under a prefix. |
| Credentials | Standard AWS provider chain (env / instance role) | Never in config; the EC2 router and Fargate tasks use IAM roles (3b-2c). |
| `push` atomicity | delete-then-put (NOT atomic) | Acceptable for our usage (per-turn session/memory, effectively one writer per key). Noted, not locked. |
| Build-without-feature | `store = "s3"` + binary lacks `--features s3` → fail fast | Clear actionable error rather than a confusing missing-variant. |
| Testing | Gated integration test against **LocalStack** + pure unit tests for key mapping | No real AWS in CI; same gated pattern as the docker-flow test. |

## Config

Add to `src/config.rs`:
- `StoreKind::S3` (alongside `Filesystem`).
- An `S3Config` struct on `DeploymentConfig`:

```toml
[deployment]
store = "s3"

[deployment.s3]
bucket = "cica-state-prod"      # required
region = "eu-west-1"            # optional; falls back to the AWS default chain
prefix = "cica"                 # optional; namespace within the bucket
endpoint = "http://localhost:4566"  # optional; for LocalStack/MinIO/testing
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    pub bucket: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}
```
`DeploymentConfig` gains `#[serde(default)] pub s3: Option<S3Config>`. (`S3Config` is plain config and always compiles; only the `S3StateStore` impl is feature-gated.)

## Components

### `S3StateStore` (`src/sandbox/state/s3.rs`, `#[cfg(feature = "s3")]`)

```rust
pub struct S3StateStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String, // normalized, no leading/trailing slash; "" if unset
}

impl S3StateStore {
    pub async fn connect(cfg: &crate::config::S3Config) -> Result<Self> { /* aws-config + endpoint override */ }
    fn object_key(&self, key: &str, rel: &str) -> String { /* "<prefix>/<key>/<rel>" sans empty segments */ }
    fn list_prefix(&self, key: &str) -> String { /* "<prefix>/<key>/" */ }
}

#[async_trait]
impl StateStore for S3StateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        // list_objects_v2 under list_prefix(key) (paginated). If no objects → Ok(false).
        // else clear_dir(dest); for each object: get_object → write dest/<rel>
        //   (rel = object key with the prefix stripped). Ok(true).
    }
    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        // list+delete all objects under list_prefix(key) (replace semantics).
        // walk src; for each file put_object(object_key(key, rel), body).
    }
}
```

- **Relative-path ↔ object-key mapping** is extracted into pure functions (`object_key`, and a `strip_prefix` inverse) so they're unit-testable without S3.
- Uses the existing `clear_dir` helper for `dest`. Directory walking mirrors `copy_dir_all`'s traversal.
- Pagination: `list_objects_v2` is followed via continuation tokens until complete.
- Deletion: batch via `delete_objects` (up to 1000 keys/request), looping for larger sets.

### Wiring — `default_store` (`src/sandbox/state/mod.rs`)

```rust
pub fn default_store(config: &Config) -> Result<Option<Arc<dyn StateStore>>> {
    match config.deployment.store {
        None => Ok(None),
        Some(StoreKind::Filesystem) => Ok(Some(Arc::new(FilesystemStateStore::new(resolved_state_path(config)?)))),
        Some(StoreKind::S3) => {
            #[cfg(feature = "s3")]
            {
                let s3 = config.deployment.s3.clone()
                    .ok_or_else(|| anyhow::anyhow!("`store = s3` requires a [deployment.s3] section"))?;
                let store = futures::executor::block_on(s3::S3StateStore::connect(&s3))?; // see note
                Ok(Some(Arc::new(store)))
            }
            #[cfg(not(feature = "s3"))]
            {
                anyhow::bail!("`store = s3` requires the binary to be built with `--features s3`")
            }
        }
    }
}
```
> Connection note: `default_store` is sync today. `S3StateStore::connect` is async (aws-config). Resolve during implementation by the cleanest option — either make `default_store` async (it's called from `try_default_provider`/`cmd::worker`, both already in async contexts) or construct the client lazily. Prefer making `default_store`/`try_default_provider` async; avoid `block_on`. (The block_on above is illustrative, not prescriptive.)

## Data flow (unchanged shape, S3 underneath)

```
router/worker → StateStore::push(src_dir, "session/<id>") → S3: delete <prefix>/session/<id>/* ; put each file
              → StateStore::pull("session/<id>", dest)    → S3: list <prefix>/session/<id>/* ; get each → dest/<rel>
```
Sessions, memories, and the `turns/<id>/{job,result}` blobs (from 3a) all flow through this identically — so the worker-dispatch protocol works over S3 with zero protocol changes.

## Error handling

- **Absent key** (no objects under the prefix) → `pull` returns `false` (not an error) — matches filesystem.
- **S3 API errors** (auth, network, missing bucket) → propagate as `Err` with context; surfaced as a turn error by the existing pipeline.
- **Partial push interrupted** → leaves partial objects (non-atomic, documented); the next successful push (delete-then-put) reconciles.
- **Missing `[deployment.s3]` with `store = s3`** → clear config error.

## Testing strategy

- **Unit (no AWS):** `object_key`/`strip_prefix` mapping (prefix handling, nested rel paths, empty prefix); config parse of `[deployment.s3]`.
- **Integration against LocalStack (gated):** a test gated by `CICA_S3_IT=1` (+ `CICA_S3_ENDPOINT`, test bucket) that runs the `StateStore` contract against a real S3 API: `push` then `pull` round-trips a nested tree byte-for-byte; `pull` of an absent key → `false`; `push` overwrites prior contents (replace). Skipped in normal `cargo test`.
- **CI:** a `s3-store` job that runs LocalStack as a service container, creates the test bucket, and runs the gated test with `--features s3`. An `--all-features` build/clippy step ensures the S3 code compiles + lints even in the fast lane.

## Distribution impact

- Default `cargo build` + `install.sh` unchanged — `aws-sdk-s3`/`aws-config` are optional, pulled only by `--features s3`.
- 3b-2c / release: cloud artifacts are built with `--features s3` (and later `fargate`); the worker image (3b-2b) is built with the cloud features. The lean default binary remains the `curl | sh` artifact.

## Out of scope (later)

- `FargateLauncher`, the cloud worker config/secrets contract, RunTask/result-return → 3b-2b.
- The `sprout` CDK (bucket/IAM/task-def/networking) → 3b-2c.
- GCS store → 3b-3.
- Atomic push / locking; `prefix`-level lifecycle/GC of orphaned `turns/<id>/` blobs (revisit if needed).
