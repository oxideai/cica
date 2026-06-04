# Phase 3b-2a: `S3StateStore` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a feature-gated `S3StateStore` that implements the existing `StateStore` trait over S3, so the router and ephemeral cloud workers share durable state with no shared filesystem — keeping the default build lean and testable locally against LocalStack.

**Architecture:** `S3StateStore` (behind `[features] s3`) mirrors `FilesystemStateStore` semantics over S3 objects keyed `<prefix>/<key>/<rel>`. The AWS client is built lazily via `tokio::sync::OnceCell` on first use, so `default_store` stays **sync** (no churn to the per-turn dispatch hot path or the default build). `default_store` gains a feature-gated S3 arm that fails fast if `store = "s3"` and the binary lacks `--features s3`.

**Tech Stack:** Rust 2024, `aws-config` + `aws-sdk-s3` (optional, feature `s3`), `tokio` (`OnceCell`), `async-trait`, `anyhow`. `tempfile` dev-dep. LocalStack for the gated integration test.

---

## Why this is safe and incremental

Everything is additive and feature-gated. The default `cargo build` / `install.sh` pull **no** AWS SDK. `default_store` stays sync (the lazy client confines all async/AWS to the `s3` module), so `try_default_provider`, `default_provider`, and the per-turn path are unchanged. With `store` unset or `filesystem`, behavior is exactly as today.

## Background facts (verified)

- `src/sandbox/state/mod.rs`: `StateStore { async pull(&self, key:&str, dest:&Path)->Result<bool>; async push(&self, src:&Path, key:&str)->Result<()> }`. `FilesystemStateStore::pull` = `if !exists {false}; clear_dir(dest); copy_dir_all` ; `push` = `remove dst; copy_dir_all`. Helper `clear_dir(dir)` (pub(crate)) empties/creates a dir. `default_store(config: &Config) -> Result<Option<Arc<dyn StateStore>>>` matches `config.deployment.store`.
- `default_store` callers (both keep working since it stays sync): `cmd/worker.rs:19`, `src/sandbox/mod.rs:65`, plus tests at `state/mod.rs:145,153`.
- `src/config.rs`: `enum StoreKind { Filesystem }`; `DeploymentConfig { store: Option<StoreKind>, state_path, provider, docker_image }`. There is a `#[cfg(test)] mod tests`.
- `Cargo.toml` has no `[features]` table yet; `[dev-dependencies] tempfile`.

## File structure

- Modify `src/config.rs` — `StoreKind::S3` + `S3Config` struct + `s3` field on `DeploymentConfig`.
- Modify `Cargo.toml` — `[features] s3` + optional `aws-config`/`aws-sdk-s3`.
- Create `src/sandbox/state/s3.rs` (`#[cfg(feature = "s3")]`) — pure key-mapping fns + `S3StateStore`.
- Modify `src/sandbox/state/mod.rs` — register `s3` module (feature-gated) + the `default_store` S3 arm.
- Modify `.github/workflows/ci.yml` — `s3-store` job (LocalStack) + an `--features s3` lint/build.

---

### Task 1: Config — `StoreKind::S3` + `S3Config`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/config.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn store_parses_s3_with_section() {
        let toml = r#"
            [deployment]
            store = "s3"
            [deployment.s3]
            bucket = "cica-state"
            region = "eu-west-1"
            prefix = "cica"
            endpoint = "http://localhost:4566"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
        let s3 = cfg.deployment.s3.unwrap();
        assert_eq!(s3.bucket, "cica-state");
        assert_eq!(s3.region.as_deref(), Some("eu-west-1"));
        assert_eq!(s3.prefix.as_deref(), Some("cica"));
        assert_eq!(s3.endpoint.as_deref(), Some("http://localhost:4566"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::store_parses_s3_with_section`
Expected: FAIL — `StoreKind::S3` / `s3` field / `S3Config` not found.

- [ ] **Step 3: Add the variant, struct, and field**

Add `S3` to `StoreKind`:
```rust
pub enum StoreKind {
    Filesystem,
    S3,
}
```

Add the `S3Config` struct near `DeploymentConfig`:
```rust
/// S3 state-store settings (used when `store = "s3"`). Credentials come from the
/// standard AWS provider chain (env / instance role), never config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    /// Bucket name (required).
    pub bucket: String,
    /// AWS region; falls back to the default chain when unset.
    #[serde(default)]
    pub region: Option<String>,
    /// Optional key namespace within the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Optional endpoint override (LocalStack / MinIO / testing).
    #[serde(default)]
    pub endpoint: Option<String>,
}
```

Add to `DeploymentConfig` (after `docker_image`):
```rust
    /// S3 store settings (used when `store = "s3"`).
    #[serde(default)]
    pub s3: Option<S3Config>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test config::tests::store_parses_s3_with_section`
Expected: PASS. `cargo build` succeeds.

> Note: adding `StoreKind::S3` makes `default_store`'s match non-exhaustive → it won't compile until Task 4 adds the arm. Keep going; Task 4 makes the full build green. (If you want each commit to build, do Task 1's commit now — `cargo build` of the lib/bin will fail on the match; that's expected and resolved in Task 4. To keep commits green, you MAY defer this commit and land Tasks 1+4 together. Either is fine; the suite must be green by Task 4.)

- [ ] **Step 5: Commit** (or defer to land with Task 4 — see note)

```bash
git add src/config.rs
git commit -m "feat(config): add StoreKind::S3 + [deployment.s3] config"
```
End every commit with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: Cargo feature + optional deps + `s3` module with pure key-mapping

**Files:**
- Modify: `Cargo.toml`
- Create: `src/sandbox/state/s3.rs`
- Modify: `src/sandbox/state/mod.rs` (register the module)

- [ ] **Step 1: Add the feature + optional deps to `Cargo.toml`**

In `[dependencies]`, add (optional):
```toml
# S3 state store (feature "s3")
aws-config = { version = "1", optional = true }
aws-sdk-s3 = { version = "1", optional = true }
```

Add a `[features]` table (after `[dependencies]`, before `[dev-dependencies]`):
```toml
[features]
default = []
s3 = ["dep:aws-config", "dep:aws-sdk-s3"]
```

- [ ] **Step 2: Register the module (feature-gated) in `src/sandbox/state/mod.rs`**

Near the existing `pub mod filesystem;`:
```rust
#[cfg(feature = "s3")]
pub mod s3;
```

- [ ] **Step 3: Create `src/sandbox/state/s3.rs` with the pure key-mapping fns + tests**

```rust
//! S3-backed `StateStore` (feature `s3`).
//!
//! Mirrors `FilesystemStateStore` semantics over S3 objects keyed
//! `<prefix>/<key>/<relative-file-path>`. The AWS client is built lazily on
//! first use so `default_store` can stay synchronous.

/// Join non-empty path segments with `/` into an S3 object key.
/// `object_key("cica", "session/abc", "store.db") == "cica/session/abc/store.db"`
/// `object_key("", "session/abc", "x") == "session/abc/x"`
fn object_key(prefix: &str, key: &str, rel: &str) -> String {
    [prefix, key, rel]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// The list prefix (with trailing slash) for all objects under `key`.
/// `dir_prefix("cica", "session/abc") == "cica/session/abc/"`
fn dir_prefix(prefix: &str, key: &str) -> String {
    let base = [prefix, key]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    format!("{base}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_joins_and_skips_empty_prefix() {
        assert_eq!(object_key("cica", "session/abc", "store.db"), "cica/session/abc/store.db");
        assert_eq!(object_key("", "session/abc", "store.db"), "session/abc/store.db");
        assert_eq!(object_key("cica", "mem/u1", "a/b.md"), "cica/mem/u1/a/b.md");
    }

    #[test]
    fn dir_prefix_has_trailing_slash() {
        assert_eq!(dir_prefix("cica", "session/abc"), "cica/session/abc/");
        assert_eq!(dir_prefix("", "session/abc"), "session/abc/");
    }

    #[test]
    fn rel_is_object_key_minus_dir_prefix() {
        let p = dir_prefix("cica", "session/abc");
        let k = object_key("cica", "session/abc", "sub/store.db");
        assert_eq!(k.strip_prefix(&p), Some("sub/store.db"));
    }
}
```

- [ ] **Step 4: Build + test with the feature**

Run: `cargo build --features s3`
Expected: SUCCESS. This is the first time the AWS crates are pulled — it's a **slow** compile, and may surface a real issue (TLS stack conflict with the existing `rustls`/`aws-lc-rs`, or `aws-config` requiring a behavior version). If `cargo build --features s3` fails for a dependency/TLS reason, resolve it (e.g. set `aws-sdk-s3`/`aws-config` features, or align the TLS provider) and report what you changed. Do NOT add `#[allow]`.
Run: `cargo test --features s3 sandbox::state::s3::tests`
Expected: the 3 pure-fn tests pass.
Also confirm the default build is unaffected: `cargo build` (no features) succeeds and pulls no AWS crates.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/sandbox/state/mod.rs src/sandbox/state/s3.rs
git commit -m "feat(state): add s3 feature + optional aws deps + key-mapping helpers"
```

---

### Task 3: `S3StateStore` (lazy client + pull/push)

**Files:**
- Modify: `src/sandbox/state/s3.rs`

> The AWS SDK calls below target `aws-sdk-s3` 1.x. If the installed SDK version's API differs (method names, builders, `ByteStream` collection), adjust to compile + behave per the doc-comments — the **contract** (mirror filesystem semantics) is what matters. Verify with `cargo build --features s3`.

- [ ] **Step 1: Add imports + the struct + lazy connect**

At the top of `src/sandbox/state/s3.rs`:
```rust
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::config::S3Config;
use crate::sandbox::state::{StateStore, clear_dir};
```

Add the struct + lazy client:
```rust
/// `StateStore` backed by an S3 bucket. The client is built lazily on first use.
pub struct S3StateStore {
    config: S3Config,
    prefix: String, // normalized: no leading/trailing slashes
    client: OnceCell<aws_sdk_s3::Client>,
}

impl S3StateStore {
    pub fn new(config: S3Config) -> Self {
        let prefix = config
            .prefix
            .clone()
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        Self { config, prefix, client: OnceCell::new() }
    }

    async fn client(&self) -> Result<&aws_sdk_s3::Client> {
        self.client
            .get_or_try_init(|| async { build_client(&self.config).await })
            .await
    }
}

async fn build_client(cfg: &S3Config) -> Result<aws_sdk_s3::Client> {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = &cfg.region {
        loader = loader.region(aws_config::Region::new(region.clone()));
    }
    if let Some(endpoint) = &cfg.endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    let shared = loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared);
    // Path-style addressing for LocalStack/MinIO (virtual-host style needs DNS).
    if cfg.endpoint.is_some() {
        builder = builder.force_path_style(true);
    }
    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}
```

- [ ] **Step 2: Implement `StateStore`**

```rust
#[async_trait]
impl StateStore for S3StateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let client = self.client().await?;
        let bucket = &self.config.bucket;
        let prefix = dir_prefix(&self.prefix, key);

        // List all objects under the prefix (paginated).
        let mut keys: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(&prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.context("s3 list_objects_v2")?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    keys.push(k.to_string());
                }
            }
            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        if keys.is_empty() {
            return Ok(false); // absent — matches FilesystemStateStore
        }

        clear_dir(dest)?;
        for obj_key in keys {
            let rel = obj_key.strip_prefix(&prefix).unwrap_or(&obj_key);
            if rel.is_empty() {
                continue;
            }
            let out = dest.join(rel);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let resp = client
                .get_object()
                .bucket(bucket)
                .key(&obj_key)
                .send()
                .await
                .with_context(|| format!("s3 get_object {obj_key}"))?;
            let bytes = resp.body.collect().await.context("s3 body collect")?.into_bytes();
            std::fs::write(&out, bytes)?;
        }
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let client = self.client().await?;
        let bucket = &self.config.bucket;
        let prefix = dir_prefix(&self.prefix, key);

        // Replace semantics: delete everything currently under the prefix.
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(&prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.context("s3 list (pre-delete)")?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    client
                        .delete_object()
                        .bucket(bucket)
                        .key(k)
                        .send()
                        .await
                        .with_context(|| format!("s3 delete_object {k}"))?;
                }
            }
            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        // Upload every file under `src`, keyed by its path relative to `src`.
        for entry in walk_files(src)? {
            let rel = entry
                .strip_prefix(src)
                .expect("walk_files yields paths under src")
                .to_string_lossy()
                .replace('\\', "/");
            let body = aws_sdk_s3::primitives::ByteStream::from_path(&entry)
                .await
                .with_context(|| format!("reading {}", entry.display()))?;
            client
                .put_object()
                .bucket(bucket)
                .key(object_key(&self.prefix, key, &rel))
                .body(body)
                .send()
                .await
                .with_context(|| format!("s3 put_object {rel}"))?;
        }
        Ok(())
    }
}

/// Recursively collect all file paths under `dir` (empty if `dir` is absent).
fn walk_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(walk_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}
```

- [ ] **Step 3: Build with the feature**

Run: `cargo build --features s3`
Expected: SUCCESS. Adjust any SDK API mismatches (e.g. `resp.contents()` returning `&[Object]` vs `Option`, `is_truncated`/`next_continuation_token` shapes, `ByteStream::from_path`) until it compiles, preserving the documented behavior. Report any adjustments.
Run: `cargo test --features s3 sandbox::state::s3::tests` — the pure-fn tests still pass (the new code has no non-AWS unit tests; real S3 behavior is covered by Task 5's LocalStack test).

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/state/s3.rs
git commit -m "feat(state): implement S3StateStore (pull/push over S3, lazy client)"
```

---

### Task 4: Wire `default_store` (feature-gated S3 arm + fail-fast)

**Files:**
- Modify: `src/sandbox/state/mod.rs`

- [ ] **Step 1: Add the S3 arm to `default_store`**

Replace the `match config.deployment.store` body to add the `S3` arm:
```rust
pub fn default_store(config: &Config) -> Result<Option<Arc<dyn StateStore>>> {
    match config.deployment.store {
        None => Ok(None),
        Some(StoreKind::Filesystem) => Ok(Some(Arc::new(FilesystemStateStore::new(
            resolved_state_path(config)?,
        )))),
        Some(StoreKind::S3) => {
            #[cfg(feature = "s3")]
            {
                let s3 = config
                    .deployment
                    .s3
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("`store = s3` requires a [deployment.s3] section"))?;
                Ok(Some(Arc::new(s3::S3StateStore::new(s3))))
            }
            #[cfg(not(feature = "s3"))]
            {
                let _ = config;
                anyhow::bail!("`store = s3` requires the binary to be built with `--features s3`")
            }
        }
    }
}
```
(`S3StateStore::new` is sync — no `.await`, so `default_store` stays sync. The lazy client connects on first `pull`/`push`.)

- [ ] **Step 2: Add tests (feature-split)**

In `src/sandbox/state/mod.rs`'s `#[cfg(test)] mod tests`:
```rust
    #[cfg(not(feature = "s3"))]
    #[test]
    fn s3_store_requires_feature() {
        use crate::config::{Config, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        assert!(default_store(&cfg).is_err());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_store_built_lazily_when_feature_on() {
        use crate::config::{Config, S3Config, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        cfg.deployment.s3 = Some(S3Config {
            bucket: "b".into(),
            ..Default::default()
        });
        // Lazy client: building the store does not connect, so this is Ok without AWS.
        assert!(default_store(&cfg).unwrap().is_some());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_store_without_section_errors() {
        use crate::config::{Config, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        cfg.deployment.s3 = None;
        assert!(default_store(&cfg).is_err());
    }
```

- [ ] **Step 3: Build + test both feature sets**

Run: `cargo build && cargo test` — default build now compiles (match is exhaustive) and all tests pass, including `s3_store_requires_feature`.
Run: `cargo build --features s3 && cargo test --features s3` — the s3-on tests pass too.

- [ ] **Step 4: Commit (with Task 1 if deferred)**

```bash
git add src/sandbox/state/mod.rs src/config.rs
git commit -m "feat(state): select S3StateStore via store = s3 (feature-gated, fail-fast)"
```

---

### Task 5: LocalStack integration test + CI

**Files:**
- Modify: `src/sandbox/state/s3.rs` (gated integration test)
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the gated LocalStack integration test**

In `src/sandbox/state/s3.rs`, add a `#[cfg(test)] mod it_tests` (or extend `tests`) with a runtime-gated test that runs the full `StateStore` contract against a real S3 endpoint. It returns early unless `CICA_S3_IT=1`.

```rust
#[cfg(test)]
mod it_tests {
    use super::*;
    use crate::config::S3Config;

    fn it_config() -> Option<S3Config> {
        if std::env::var_os("CICA_S3_IT").is_none() {
            return None; // gated: only runs in the s3-store CI job / when explicitly enabled
        }
        Some(S3Config {
            bucket: std::env::var("CICA_S3_BUCKET").unwrap_or_else(|_| "cica-test".into()),
            region: Some(std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into())),
            prefix: Some("it".into()),
            endpoint: Some(
                std::env::var("CICA_S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:4566".into()),
            ),
        })
    }

    fn write(p: &std::path::Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    #[tokio::test]
    async fn s3_round_trip_absent_and_replace() {
        let Some(cfg) = it_config() else { return };
        let store = S3StateStore::new(cfg);

        // absent → false
        let d0 = tempfile::tempdir().unwrap();
        assert!(!store.pull("session/none", d0.path()).await.unwrap());

        // push nested tree → pull round-trips byte-for-byte
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "alpha");
        write(&src.path().join("sub/b.txt"), "beta");
        store.push(src.path(), "session/x").await.unwrap();

        let d1 = tempfile::tempdir().unwrap();
        assert!(store.pull("session/x", d1.path()).await.unwrap());
        assert_eq!(std::fs::read_to_string(d1.path().join("a.txt")).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(d1.path().join("sub/b.txt")).unwrap(), "beta");

        // push replaces prior contents
        let src2 = tempfile::tempdir().unwrap();
        write(&src2.path().join("new.txt"), "new");
        store.push(src2.path(), "session/x").await.unwrap();
        let d2 = tempfile::tempdir().unwrap();
        store.pull("session/x", d2.path()).await.unwrap();
        assert!(!d2.path().join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(d2.path().join("new.txt")).unwrap(), "new");
    }
}
```

- [ ] **Step 2: (If LocalStack is available) run it for real**

If you have Docker: start LocalStack (`docker run -d -p 4566:4566 localstack/localstack`), create the bucket (`aws --endpoint-url http://localhost:4566 s3 mb s3://cica-test` or via `aws-cli`/`awslocal`), then:
`CICA_S3_IT=1 AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test cargo test --features s3 it_tests -- --nocapture`
Expected: PASS. If LocalStack/Docker is unavailable here, SKIP and note it's CI-covered (the gated test is a no-op without `CICA_S3_IT`).

- [ ] **Step 3: Add the CI `s3-store` job + `--features s3` lint**

In `.github/workflows/ci.yml`, add a job (2-space indent under `jobs:`, matching existing jobs):
```yaml
  s3-store:
    runs-on: ubuntu-latest
    services:
      localstack:
        image: localstack/localstack
        ports:
          - 4566:4566
        env:
          SERVICES: s3
    env:
      AWS_ACCESS_KEY_ID: test
      AWS_SECRET_ACCESS_KEY: test
      AWS_REGION: us-east-1
      CICA_S3_ENDPOINT: http://localhost:4566
      CICA_S3_BUCKET: cica-test
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Create test bucket
        run: |
          pip install awscli-local awscli >/dev/null 2>&1 || pip install awscli >/dev/null 2>&1
          aws --endpoint-url http://localhost:4566 s3 mb s3://cica-test
      - name: Clippy (s3 feature)
        run: cargo clippy --features s3 --all-targets -- -D warnings
      - name: S3 integration test (LocalStack)
        run: CICA_S3_IT=1 cargo test --features s3 it_tests -- --nocapture
```
> The bucket-create step uses the AWS CLI against LocalStack. If `awslocal`/`aws` install is flaky in CI, an alternative is a tiny `aws-cli` action or creating the bucket inside the test's setup; settle during implementation so the job is green. The key requirement: LocalStack reachable + bucket exists + the gated test runs with `--features s3`.

- [ ] **Step 4: Validate YAML + final gates**

Run: `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/ci.yml')); print('s3-store' in d['jobs'])"` → `True`.
Run: `cargo clippy --all-targets -- -D warnings` (default) and `cargo clippy --features s3 --all-targets -- -D warnings` — both clean.
Run: `cargo fmt` then `cargo fmt --check` — clean.
Run: `cargo test` (default — gated test is a no-op) — all pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test(state): LocalStack S3 integration test + CI s3-store job"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- Feature-gated `aws-config`/`aws-sdk-s3` + `[features] s3` → Task 2.
- `StoreKind::S3` + `S3Config` (bucket/region/prefix/endpoint), creds via AWS chain → Task 1.
- `S3StateStore` mirroring filesystem semantics over `<prefix>/<key>/<rel>`; pull→false when absent; push delete-then-put replace; pagination; lazy client → Tasks 2 (mapping), 3 (impl).
- `default_store` S3 arm + fail-fast without the feature → Task 4.
- Lazy client keeps `default_store` sync (no async ripple — supersedes the spec's "make default_store async" note) → Task 3 (`OnceCell`), Task 4 (sync arm).
- Unit tests (key mapping, config parse) + LocalStack gated integration + CI → Tasks 1, 2, 5.
- Distribution: default build pulls no AWS SDK; `--features s3` adds it; `--features s3` clippy in CI → Tasks 2, 5.
- Non-atomic push documented → spec; behavior preserved (delete-then-put) → Task 3.

**Placeholder scan:** No "TBD"/"handle errors appropriately". Every code step has complete code. The SDK-version and CLI-install notes (Tasks 3, 5) are explicit "verify against the real version, adjust API specifics" guidance for the genuinely environment-dependent bits (AWS SDK + LocalStack) — the same honest pattern used for the Dockerfile in 3b-1 — not placeholders for logic.

**Type consistency:** `object_key(prefix,key,rel)` / `dir_prefix(prefix,key)` are used consistently across the impl + tests (Tasks 2, 3). `S3StateStore::new(S3Config) -> Self` (sync) matches the `default_store` arm (Task 4). `S3Config { bucket, region, prefix, endpoint }` matches the config (Task 1), the store (Task 3), and the tests (Tasks 1, 5). `clear_dir` reused from `state/mod.rs`. The `StateStore` trait signatures match the existing trait.

## Next (after this merges)

Phase 3b-2b: `FargateLauncher` (`ecs:RunTask` + `DescribeTasks` poll) + the cloud worker config/secrets contract (env from Secrets Manager) + result-return over S3; reuses `S3StateStore` + the fake-backend harness. Then 3b-2c: the `sprout` CDK.
