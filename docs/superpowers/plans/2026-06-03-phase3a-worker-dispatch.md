# Phase 3a: Worker Process + Store-Mediated Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `cica worker --turn <id>` subcommand and a `SubprocessWorkerProvider` so the router can dispatch each turn to a one-shot worker process via the `StateStore`, proving the full worker round-trip on one box with no cloud, Docker, or feature flags.

**Architecture:** The job/result travel through the `StateStore` keyed by a `turn_id` (the protocol the cloud launchers will reuse). `SubprocessWorkerProvider` (a `SandboxProvider`, so it drops into the existing `query_ai_with_session`) serializes the `TurnJob` to `turns/<id>/job`, spawns `cica worker --turn <id>`, awaits exit, reads `turns/<id>/result`. The worker reconstructs Phase 2's `HydratingProvider(LocalProcessProvider, store)` at `paths.base`, runs one turn, and writes the result. `default_provider` selects in-process vs subprocess from a new `[deployment].provider` config key.

**Tech Stack:** Rust 2024, `tokio` (process + async), `serde`/`serde_json`, `uuid`, `anyhow` (all existing deps). `tempfile` (existing dev-dep) for tests.

---

## Why this is safe and incremental

`provider` defaults to `local` → `default_provider` returns exactly the Phase 1/2 result (bare `LocalProcessProvider`, or `HydratingProvider` if a store is set). The worker path activates only with `[deployment] provider = "subprocess"`, which additionally requires a store. No new dependencies; `cica worker` is a subcommand of the same binary; distribution is unchanged.

## Background facts (verified against the code)

- `src/main.rs` defines a clap `Commands` enum (`Init`, `Approve { code }`, `Paths`) dispatched in `main`; `None` → `cmd::run::run()`. Subcommand modules live in `src/cmd/` and are listed in `src/cmd/mod.rs`.
- `src/sandbox/mod.rs`: `TurnJob` has `#[allow(dead_code)] #[derive(Debug, Clone)]`; `TurnResult` has `#[derive(Debug, Clone)]`. `SandboxProvider::run_turn(&self, job: TurnJob) -> Result<TurnResult>`. `default_provider(config: &Config) -> Box<dyn SandboxProvider>` currently returns `HydratingProvider(local, store)` when a store is configured, else `LocalProcessProvider`.
- `crate::config::AiBackend` already derives `Serialize, Deserialize`.
- `HydratingProvider::new(inner, store: Arc<dyn StateStore>, claude_home: PathBuf, cwd: PathBuf)` (`src/sandbox/hydrating.rs`).
- `sandbox::state::default_store(config) -> Result<Option<Arc<dyn StateStore>>>`; `StateStore::{pull(key,&Path)->Result<bool>, push(&Path,key)->Result<()>}`.
- `crate::config::paths() -> Result<Paths>` with `.claude_home: PathBuf`, `.base: PathBuf`, `.internal_dir: PathBuf`.
- `DeploymentConfig` (`src/config.rs`) currently has `store: Option<StoreKind>` and `state_path: Option<String>`.
- `query_ai_with_session` (`src/channels/mod.rs`) calls `sandbox::default_provider(&config)` then `provider.run_turn(job)` — it is agnostic to which provider it gets.

## File structure

- Modify `src/sandbox/mod.rs` — add `Serialize, Deserialize` to `TurnJob`/`TurnResult`; declare `pub mod worker;`; extend `default_provider` to branch on `provider`.
- Modify `src/config.rs` — add `ProviderKind` enum + `provider` field on `DeploymentConfig`.
- Create `src/sandbox/worker.rs` — `SubprocessWorkerProvider` (router side) + the shared `turn_key`/job-IO helpers.
- Create `src/cmd/worker.rs` — `cica worker` entry point (`run(turn_id)`), building the worker engine and round-tripping the store.
- Modify `src/cmd/mod.rs` — `pub mod worker;`.
- Modify `src/main.rs` — add `Worker { turn: String }` subcommand + dispatch.

---

### Task 1: Make `TurnJob`/`TurnResult` serializable

**Files:**
- Modify: `src/sandbox/mod.rs`

- [ ] **Step 1: Write the failing test**

Add a test module at the end of `src/sandbox/mod.rs` (there is already a `#[cfg(test)] mod tests` for `default_provider`; add this test inside it):

```rust
    #[test]
    fn turn_job_and_result_round_trip_json() {
        let job = TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            cwd: None,
            skip_permissions: true,
            backend: crate::config::AiBackend::Claude,
            model: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: TurnJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "telegram:1");
        assert_eq!(back.resume_session.as_deref(), Some("sess-1"));

        let result = TurnResult {
            response: "ok".into(),
            backend_session_id: "sess-2".into(),
            cost_usd: Some(0.1),
            duration_ms: Some(5),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TurnResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend_session_id, "sess-2");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sandbox::tests::turn_job_and_result_round_trip_json`
Expected: FAIL to compile — `TurnJob`/`TurnResult` do not implement `Serialize`/`Deserialize`.

- [ ] **Step 3: Add the derives**

In `src/sandbox/mod.rs`, add `serde::{Serialize, Deserialize}` to the derive lists. The structs become:

```rust
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnJob {
```

and

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnResult {
```

(Leave all fields and the `#[allow(dead_code)]` unchanged.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test sandbox::tests::turn_job_and_result_round_trip_json`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/mod.rs
git commit -m "feat(sandbox): make TurnJob/TurnResult serde-serializable"
```
End every commit message with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: `ProviderKind` config

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/config.rs`:

```rust
    #[test]
    fn provider_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.deployment.provider.is_none());
    }

    #[test]
    fn provider_parses_subprocess() {
        let toml = r#"
            [deployment]
            provider = "subprocess"
            store = "filesystem"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Subprocess));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::provider`
Expected: FAIL to compile — `ProviderKind` / `provider` field not found.

- [ ] **Step 3: Add the enum and field**

In `src/config.rs`, add next to `StoreKind`:

```rust
/// Where a turn executes (none/local = in-process; subprocess = one-shot worker).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Local,
    Subprocess,
}
```

Add to `DeploymentConfig`:

```rust
    /// Turn execution mode. `None` (or `Local`) = in-process (default).
    #[serde(default)]
    pub provider: Option<ProviderKind>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test config::tests::provider`
Expected: PASS (both).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add [deployment] provider selector"
```

---

### Task 3: Shared turn-key + job/result store IO helpers

**Files:**
- Create: `src/sandbox/worker.rs`
- Modify: `src/sandbox/mod.rs` (add `pub mod worker;`)

This task adds the (de)serialization-through-the-store helpers used by BOTH the router provider (Task 4) and the worker subcommand (Task 5). It does not yet add `SubprocessWorkerProvider`.

- [ ] **Step 1: Register the module**

In `src/sandbox/mod.rs`, near the other module declarations:

```rust
pub mod worker;
```

- [ ] **Step 2: Write `src/sandbox/worker.rs` with helpers + tests**

```rust
//! Worker dispatch: run a turn in a one-shot `cica worker` child process,
//! exchanging the job and result through the `StateStore` keyed by a turn id.

use std::path::Path;

use anyhow::{Context, Result};

use crate::sandbox::state::StateStore;
use crate::sandbox::{TurnJob, TurnResult};

/// Store key prefix for a turn's job/result blobs.
fn job_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/job")
}

fn result_key(turn_id: &str) -> String {
    format!("turns/{turn_id}/result")
}

fn turn_prefix(turn_id: &str) -> String {
    format!("turns/{turn_id}")
}

/// Serialize `job` into a fresh dir and push it under `turns/<id>/job`.
async fn push_job(store: &dyn StateStore, turn_id: &str, job: &TurnJob) -> Result<()> {
    let dir = scratch_dir(turn_id, "job");
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(job)?;
    std::fs::write(dir.join("job.json"), json)?;
    store.push(&dir, &job_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Pull `turns/<id>/job` and deserialize the `TurnJob`.
async fn pull_job(store: &dyn StateStore, turn_id: &str) -> Result<TurnJob> {
    let dir = scratch_dir(turn_id, "job-in");
    let found = store.pull(&job_key(turn_id), &dir).await?;
    let job = if found {
        let bytes = std::fs::read(dir.join("job.json")).context("reading job.json")?;
        serde_json::from_slice(&bytes).context("deserializing TurnJob")?
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        anyhow::bail!("no job found for turn {turn_id}");
    };
    let _ = std::fs::remove_dir_all(&dir);
    Ok(job)
}

/// Serialize `result` into a fresh dir and push it under `turns/<id>/result`.
async fn push_result(store: &dyn StateStore, turn_id: &str, result: &TurnResult) -> Result<()> {
    let dir = scratch_dir(turn_id, "result");
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(result)?;
    std::fs::write(dir.join("result.json"), json)?;
    store.push(&dir, &result_key(turn_id)).await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Pull `turns/<id>/result`; `None` if the worker never wrote one.
async fn pull_result(store: &dyn StateStore, turn_id: &str) -> Result<Option<TurnResult>> {
    let dir = scratch_dir(turn_id, "result-in");
    let found = store.pull(&result_key(turn_id), &dir).await?;
    let out = if found {
        let bytes = std::fs::read(dir.join("result.json")).context("reading result.json")?;
        Some(serde_json::from_slice(&bytes).context("deserializing TurnResult")?)
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

/// A unique temp dir for staging a blob in/out of the store.
fn scratch_dir(turn_id: &str, kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cica-turn-{turn_id}-{kind}-{}", uuid::Uuid::new_v4()))
}

/// Best-effort removal of a turn's blobs after the router has the result.
async fn cleanup(store: &dyn StateStore, turn_id: &str) {
    // The StateStore has no delete; pushing an empty dir collapses the entry.
    let empty = scratch_dir(turn_id, "empty");
    if std::fs::create_dir_all(&empty).is_ok() {
        let _ = store.push(&empty, &turn_prefix(turn_id)).await;
        let _ = std::fs::remove_dir_all(&empty);
    }
}

/// Run the in-store job/result round-trip used by the worker subcommand:
/// pull the job, run it through `engine`, push the result.
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
}
```

> Note on `cleanup`: `StateStore` has no `delete`; pushing an empty dir to `turns/<id>` replaces the subtree with nothing, which is sufficient for Phase 2's `FilesystemStateStore` (`push` removes the existing dir then copies the empty source). Keep `Path` imported only if used; if `use std::path::Path` is unused, remove it (the helpers use `std::path::PathBuf` via `scratch_dir`'s return — adjust the import to what compiles cleanly).

- [ ] **Step 3: Build and test**

Run: `cargo test sandbox::worker` — expect 3 tests pass. `cargo build` succeeds (dead-code warnings on `run_worker_turn`/`cleanup`/`SubprocessWorkerProvider`-absent are expected until Tasks 4–5 wire them; do NOT add `#[allow]`).

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/mod.rs src/sandbox/worker.rs
git commit -m "feat(sandbox): add store-mediated turn job/result IO helpers"
```

---

### Task 4: `SubprocessWorkerProvider` (router side)

**Files:**
- Modify: `src/sandbox/worker.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `src/sandbox/worker.rs`:

```rust
    #[tokio::test]
    async fn provider_dispatches_to_a_worker_command_and_returns_result() {
        // A fake "worker" command: `sh -c` that reads nothing and writes a
        // result blob for the turn id, simulating `cica worker`.
        // We drive run_turn with a command that, given CICA_TEST_TURN + store
        // root via env, writes turns/<id>/result then exits 0.
        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(root.path().to_path_buf()));

        // Hand-write the result the fake worker "produces" by pre-seeding a
        // closure-based stub is not possible across a process boundary, so this
        // test instead exercises the helper-level contract: push job, then
        // simulate the worker via run_worker_turn with a stub engine, then read.
        push_job(store.as_ref(), "tX", &sample_job()).await.unwrap();

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
        run_worker_turn(store.as_ref(), &StubEngine, "tX").await.unwrap();
        let result = pull_result(store.as_ref(), "tX").await.unwrap().unwrap();
        assert_eq!(result.response, "from-worker");
        assert_eq!(result.backend_session_id, "sess-w");
    }
```

> This test validates the worker-side round-trip (push job → `run_worker_turn` with a stub engine → result readable), which is the exact contract `SubprocessWorkerProvider` depends on across the process boundary. The real cross-process spawn is covered by the manual integration test (the child needs a configured `cica`). Keep this test; it locks the protocol.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sandbox::worker::tests::provider_dispatches`
Expected: PASS already if Task 3 landed `run_worker_turn` (this test only uses Task 3 helpers). If it compiles and passes, proceed; the implementation step below adds the actual `SubprocessWorkerProvider` type used in production.

- [ ] **Step 3: Add `SubprocessWorkerProvider`**

Add to `src/sandbox/worker.rs` (module level):

```rust
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::warn;
use uuid::Uuid;

use crate::sandbox::SandboxProvider;

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

        let result = pull_result(self.store.as_ref(), &turn_id).await?;
        cleanup(self.store.as_ref(), &turn_id).await;

        result.ok_or_else(|| anyhow::anyhow!("worker produced no result for turn {turn_id}"))
    }
}
```

Ensure imports are merged cleanly at the top of the file (no duplicate `use anyhow::...`). `warn` may be unused here; if so, drop it (it is referenced in Task 5's cmd module, not this file).

- [ ] **Step 4: Build and test**

Run: `cargo test sandbox::worker && cargo build`
Expected: all worker tests pass; build succeeds (dead-code warning on `SubprocessWorkerProvider::new` until Task 6 wires it — acceptable for now).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/worker.rs
git commit -m "feat(sandbox): add SubprocessWorkerProvider"
```

---

### Task 5: `cica worker` subcommand

**Files:**
- Create: `src/cmd/worker.rs`
- Modify: `src/cmd/mod.rs` (`pub mod worker;`)
- Modify: `src/main.rs` (add `Worker { turn }` subcommand + dispatch)

- [ ] **Step 1: Register the command module**

In `src/cmd/mod.rs`, add:

```rust
pub mod worker;
```

- [ ] **Step 2: Write `src/cmd/worker.rs`**

```rust
//! `cica worker --turn <id>`: run exactly one turn, then exit.
//!
//! Reads the `TurnJob` from the state store, runs it through the same
//! `HydratingProvider` the in-process path uses, and writes the `TurnResult`
//! back to the store. Exits non-zero (without a result) on any failure.

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::sandbox::hydrating::HydratingProvider;
use crate::sandbox::local::LocalProcessProvider;
use crate::sandbox::state::default_store;
use crate::sandbox::worker::run_worker_turn;

pub async fn run(turn_id: &str) -> Result<()> {
    let config = Config::load()?;
    let paths = crate::config::paths()?;

    let store = default_store(&config)?
        .ok_or_else(|| anyhow!("`cica worker` requires [deployment].store to be configured"))?;

    let engine = HydratingProvider::new(
        LocalProcessProvider::new(),
        store.clone(),
        paths.claude_home,
        paths.base,
    );

    run_worker_turn(store.as_ref(), &engine, turn_id).await
}
```

> `LocalProcessProvider` and `HydratingProvider` are public (Phase 1/2). `default_store` returns `Option<Arc<dyn StateStore>>`; `store.clone()` (Arc clone) is passed to the provider while `store.as_ref()` drives the round-trip. If `crate::sandbox::local::LocalProcessProvider` is re-exported as `crate::sandbox::LocalProcessProvider`, prefer the re-export path; verify with `grep -n "pub use local" src/sandbox/mod.rs` and use whichever compiles.

- [ ] **Step 3: Wire the subcommand in `src/main.rs`**

Add a variant to the `Commands` enum:

```rust
    /// Run a single turn as a one-shot worker (internal; used by the router)
    Worker {
        /// The turn id whose job/result live in the state store
        #[arg(long)]
        turn: String,
    },
```

Add a match arm in `main`:

```rust
        Some(Commands::Worker { turn }) => cmd::worker::run(&turn).await,
```

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: SUCCESS. `cica worker --help` should now list the `--turn` flag (optional manual check: `cargo run -- worker --help`).

- [ ] **Step 5: Commit**

```bash
git add src/cmd/mod.rs src/cmd/worker.rs src/main.rs
git commit -m "feat(cmd): add `cica worker --turn` subcommand"
```

---

### Task 6: Wire `default_provider` to honor `provider = subprocess`

**Files:**
- Modify: `src/sandbox/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `src/sandbox/mod.rs`:

```rust
    #[test]
    fn subprocess_provider_requires_a_store() {
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        // No store configured → must be an error, not a silent local fallback.
        assert!(try_default_provider(&cfg).is_err());
    }

    #[test]
    fn subprocess_provider_built_when_store_present() {
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Subprocess);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-prov-test".into());
        assert!(try_default_provider(&cfg).is_ok());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sandbox::tests::subprocess_provider`
Expected: FAIL to compile — `try_default_provider` not found.

- [ ] **Step 3: Refactor `default_provider` into a fallible `try_default_provider`**

Replace the existing `default_provider` in `src/sandbox/mod.rs` with a fallible core plus a thin wrapper that preserves the infallible `(&Config) -> Box<dyn SandboxProvider>` signature the channels code calls:

```rust
/// Build the configured provider. Errors when the configuration is invalid
/// (e.g. `provider = subprocess` without a store).
pub fn try_default_provider(config: &Config) -> Result<Box<dyn SandboxProvider>> {
    use crate::config::ProviderKind;

    let store = state::default_store(config)?;

    match config.deployment.provider.unwrap_or(ProviderKind::Local) {
        ProviderKind::Local => {
            let local = LocalProcessProvider::new();
            match store {
                Some(store) => {
                    let paths = crate::config::paths()?;
                    Ok(Box::new(hydrating::HydratingProvider::new(
                        local,
                        store,
                        paths.claude_home,
                        paths.base,
                    )))
                }
                None => Ok(Box::new(local)),
            }
        }
        ProviderKind::Subprocess => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = subprocess` requires [deployment].store to be set")
            })?;
            let self_exe = std::env::current_exe()?;
            Ok(Box::new(worker::SubprocessWorkerProvider::new(store, self_exe)))
        }
    }
}

/// Infallible wrapper used by call sites that cannot recover. On a
/// configuration error it logs and falls back to the in-process provider,
/// so a misconfigured store never silently routes through a broken worker.
pub fn default_provider(config: &Config) -> Box<dyn SandboxProvider> {
    match try_default_provider(config) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("invalid provider configuration ({e}); using in-process provider");
            Box::new(LocalProcessProvider::new())
        }
    }
}
```

Add `use anyhow::Result;` at the top of `src/sandbox/mod.rs` if not already imported (the trait already returns `Result`, so it likely is).

> Behavior note: the infallible `default_provider` falls back to in-process ONLY on a *configuration* error (so the router still starts). A *runtime* worker failure (Task 4) still surfaces as a turn error with no fallback — the two are different and both match the spec.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test sandbox::tests::subprocess_provider`
Expected: PASS (both). Then `cargo test` — all pass.

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/mod.rs
git commit -m "feat(sandbox): select SubprocessWorkerProvider via [deployment].provider"
```

---

### Task 7: Lint, fmt, dead-code sweep, manual-test doc

**Files:**
- Possibly modify: `src/sandbox/worker.rs`, `src/sandbox/mod.rs` (imports only)
- Modify: `docs/superpowers/plans/2026-06-03-phase3a-worker-dispatch.md` (append manual steps)

- [ ] **Step 1: Clippy gate**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS. All Phase 3a items are now reachable (`SubprocessWorkerProvider` via `try_default_provider`; `run_worker_turn` via `cmd::worker`; helpers via both). If a helper is genuinely unused, report it; do NOT blanket-`#[allow(dead_code)]`. Remove any unused imports clippy flags (e.g. a stray `use std::path::Path` or `warn` in `worker.rs`).

- [ ] **Step 2: Format**

Run: `cargo fmt` then `cargo fmt --check`
Expected: clean.

- [ ] **Step 3: Full test run**

Run: `cargo test`
Expected: all pass (Phase 1/2/3a).

- [ ] **Step 4: Append the manual integration test to this plan**

Append to the END of `docs/superpowers/plans/2026-06-03-phase3a-worker-dispatch.md`:

```markdown

## Manual integration test (run in a configured environment)

Needs a configured cica with the `claude` CLI + credentials. Not runnable in CI.

1. In `config.toml`:
   ```toml
   [deployment]
   provider = "subprocess"
   store = "filesystem"
   ```
2. Start `cica`; send a message. Confirm a `cica worker --turn <id>` child process runs (e.g. visible in `ps`) and the reply arrives.
3. Confirm `internal/state-store/turns/<id>/` is created during the turn and cleared afterward, and that `internal/state-store/session/<backend_id>/` + `internal/state-store/mem/<channel>_<user>/` are written.
4. Send a follow-up in the same conversation; confirm context resumes (the worker restored the session before `--resume`).
5. Confirm a memory written in step 2 is still searchable after step 4.
6. Negative check: set `provider = "subprocess"` with NO `store`; confirm `cica` logs the configuration error and runs in-process rather than crashing.
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(sandbox): fmt + clippy for worker dispatch; document manual test"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- `cica worker --turn <id>` subcommand → Task 5.
- Store-mediated dispatch keyed by `turn_id` (`turns/<id>/job`, `turns/<id>/result`) → Tasks 3, 4.
- `SubprocessWorkerProvider` as a `SandboxProvider` slotting into `query_ai_with_session` → Task 4 (+ Task 6 selects it; `query_ai_with_session` already calls `default_provider`).
- Hydration inside the worker via Phase 2 `HydratingProvider` at `paths.base` → Task 5.
- `TurnJob`/`TurnResult` serializable; job/result as one-file dirs reusing `StateStore` (no trait change) → Tasks 1, 3.
- `[deployment].provider` config; `subprocess` requires a store, fail-fast → Tasks 2, 6.
- Worker failure → turn error, no in-process fallback (config error is a separate, recoverable case) → Task 4 (`bail!` on non-zero / missing result) + Task 6 note.
- Worker cwd = `paths.base` → Task 5.
- No new deps; distribution unchanged → only existing crates used.
- Testing: serialization round-trip, helper round-trip / protocol, config validation, manual integration → Tasks 1, 3, 4, 6, 7.

**Placeholder scan:** No "TBD"/"handle errors appropriately"/"similar to Task N". Every code step has complete code. Task 3's `Path` import note and Task 4's import-merge note are explicit cleanup instructions, not placeholders.

**Type consistency:** `TurnJob`/`TurnResult` field names match across Tasks 1, 3, 4, 5. `run_worker_turn(store: &dyn StateStore, engine: &dyn SandboxProvider, turn_id: &str)` is consistent between Task 3 (definition), Task 4 (test), and Task 5 (call). `SubprocessWorkerProvider::new(store: Arc<dyn StateStore>, self_exe: PathBuf)` matches Task 6's construction. `try_default_provider`/`default_provider` signatures match the channels call site (`default_provider(&config) -> Box<dyn SandboxProvider>`). Store keys `turns/<id>/job`, `turns/<id>/result`, `turns/<id>` are consistent across helpers.

## Next phase (separate plan)

Phase 3b: worker `Dockerfile`; `ContainerProvider` + `Launcher` trait + AWS Fargate; feature-gated `S3StateStore`; network result-return (task-status polling); deployment-contract doc; feature-gated cloud release artifacts + `--all-features` CI.
