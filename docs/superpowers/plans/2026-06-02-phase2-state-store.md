# Phase 2: StateStore + Hydrate/Dehydrate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and prove a capture/restore round-trip for the durable state a turn touches — the Claude session files and the user's `memories/` — via a `StateStore` trait, a `FilesystemStateStore`, and a `HydratingProvider` decorator, all off by default and added with zero new runtime dependencies.

**Architecture:** A new `src/sandbox/state/` module defines a `StateStore` trait (`pull`/`push` directory trees by key) and a `FilesystemStateStore`. `src/sandbox/artifacts.rs` maps a Claude `session_id` to its on-disk files and captures/restores them, keyed by `session_id` and restored under the slug of the current cwd (so a future remote worker with a different cwd still resumes). `src/sandbox/hydrating.rs` wraps any `SandboxProvider`: pull session + memories → run inner turn → capture + push. `default_provider` composes the decorator only when a store is configured; with none configured it returns the bare `LocalProcessProvider` (today's behavior, untouched).

**Tech Stack:** Rust 2024, `tokio`, `async-trait`, `anyhow`, `uuid` (existing deps); `tempfile` added as a dev-dependency for tests. No cloud SDKs.

---

## Why this is safe and incremental

Everything is additive and gated behind a new, optional `[deployment].store` config key. With no store configured, `default_provider` returns the same `LocalProcessProvider` as Phase 1 — identical runtime behavior, no new dependencies in the shipped binary. The store, when enabled, runs on the same box; its only purpose is to validate the round-trip mechanism Phase 3 will rely on.

## Background facts (verified against the code)

- The Claude subprocess runs with `HOME = paths.claude_home` (`internal/claude-home`) and `cwd = job.cwd.unwrap_or(paths.base)` (`src/backends/claude.rs:102-105`). In Phase 1/2 `job.cwd` is always `None`, so the effective cwd is `paths.base`.
- A Claude session's files live under `$CLAUDE_HOME/.claude/`:
  - transcript: `projects/<slug(cwd)>/<session_id>.jsonl`
  - session env: `session-env/<session_id>` (file or dir)
  - todos: `todos/<session_id>-agent-*.json`
- The slug rule (from on-disk evidence): every non-alphanumeric character of the cwd string becomes `-`. Example: `/Users/dcvz/Library/Application Support/cica` → `-Users-dcvz-Library-Application-Support-cica`.
- Per-user memories live at `paths.base/users/{channel}_{user_id}/memories/` (flat markdown). `crate::memory::memories_dir(channel, user_id)` returns this.
- `TurnJob` (Phase 1, `src/sandbox/mod.rs`) carries `session_id` (logical "telegram:42"), `channel`, `user_id`, `resume_session: Option<String>` (the backend session id to resume), `backend: AiBackend`, etc. `TurnResult.backend_session_id` is the backend session id after the turn.
- **Store keys:** sessions are keyed by the **backend session id** (the `<uuid>` Claude uses for its files), `session/<backend_session_id>`. Memories are keyed `mem/<channel>_<user_id>`. The logical→backend mapping already lives in `PairingStore.sessions` (router-side); the store never needs it.

## File structure

- Create `src/sandbox/state/mod.rs` — `StateStore` trait, `default_store` factory, `pub(crate)` fs helpers (`copy_dir_all`, `clear_dir`, `copy_path`, `safe_join`).
- Create `src/sandbox/state/filesystem.rs` — `FilesystemStateStore`.
- Create `src/sandbox/artifacts.rs` — `claude_project_slug`, `ClaudeSessionArtifacts::{capture, restore}`.
- Create `src/sandbox/hydrating.rs` — `HydratingProvider`.
- Modify `src/sandbox/mod.rs` — declare the new modules, extend `default_provider`, re-exports.
- Modify `src/config.rs` — add `DeploymentConfig` + `StoreKind`, and a `deployment` field on `Config`.
- Modify `Cargo.toml` — add `tempfile` to `[dev-dependencies]`.

---

### Task 1: Config surface — `DeploymentConfig` + `StoreKind`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/config.rs` (create the block at the end of the file if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_defaults_to_no_store() {
        let cfg = Config::default();
        assert!(cfg.deployment.store.is_none());
    }

    #[test]
    fn deployment_parses_filesystem_store() {
        let toml = r#"
            backend = "claude"
            [deployment]
            store = "filesystem"
            state_path = "/tmp/cica-state"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::Filesystem));
        assert_eq!(cfg.deployment.state_path.as_deref(), Some("/tmp/cica-state"));
    }
}
```

(If a `#[cfg(test)] mod tests` already exists in `src/config.rs`, add only the two test fns and reuse the existing `use super::*;`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::tests::deployment`
Expected: FAIL to compile — `deployment` field / `DeploymentConfig` / `StoreKind` not found.

- [ ] **Step 3: Add the types and field**

Add near the `AiBackend` enum in `src/config.rs`:

```rust
/// Which durable state store to use (none = all-local, today's behavior).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Filesystem,
}

/// Distributed-deployment configuration. All optional; absent = single-box.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeploymentConfig {
    /// State store backend. `None` disables hydration (default).
    #[serde(default)]
    pub store: Option<StoreKind>,
    /// Filesystem store root. Defaults to `internal/state-store` when unset.
    #[serde(default)]
    pub state_path: Option<String>,
}
```

Then add a field to the `Config` struct (alongside `backend`, `audit`, etc.):

```rust
    /// Distributed-deployment settings (state store, etc.)
    #[serde(default)]
    pub deployment: DeploymentConfig,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config::tests::deployment`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add optional [deployment] store config"
```
End every commit message with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: `StateStore` trait + fs helpers + `FilesystemStateStore`

**Files:**
- Create: `src/sandbox/state/mod.rs`
- Create: `src/sandbox/state/filesystem.rs`
- Modify: `src/sandbox/mod.rs` (add `pub mod state;`)
- Modify: `Cargo.toml` (`[dev-dependencies] tempfile`)

- [ ] **Step 1: Add the dev-dependency**

In `Cargo.toml`, add a `[dev-dependencies]` section (or append to it if present):

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Register the module**

In `src/sandbox/mod.rs`, add near the existing `mod local;` line:

```rust
pub mod state;
```

- [ ] **Step 3: Write `src/sandbox/state/mod.rs` with the trait, helpers, and tests**

```rust
//! Durable state storage for sessions and memories.
//!
//! Phase 2 provides only `FilesystemStateStore`. Later phases add
//! feature-gated S3/GCS backends behind the same `StateStore` trait.

pub mod filesystem;

pub use filesystem::FilesystemStateStore;

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::config::{Config, StoreKind};

/// A durable store of directory trees, addressed by string keys.
///
/// Keys may contain `/` to namespace entries (e.g. `session/<id>`).
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Replace `dest`'s contents with what is stored under `key`.
    /// Returns `false` (and leaves `dest` untouched) if `key` is absent.
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool>;
    /// Store the contents of `src` under `key`, replacing any prior contents.
    async fn push(&self, src: &Path, key: &str) -> Result<()>;
}

/// Build the configured store, or `None` if deployment.store is unset.
pub fn default_store(config: &Config) -> Result<Option<Arc<dyn StateStore>>> {
    match config.deployment.store {
        None => Ok(None),
        Some(StoreKind::Filesystem) => {
            let root = match &config.deployment.state_path {
                Some(p) => PathBuf::from(p),
                None => crate::config::paths()?.internal_dir.join("state-store"),
            };
            Ok(Some(Arc::new(FilesystemStateStore::new(root))))
        }
    }
}

/// Join `key` onto `root`, rejecting `..` and normalizing each segment to
/// path-safe characters. Prevents traversal outside `root`.
pub(crate) fn safe_join(root: &Path, key: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            bail!("invalid state key segment: ..");
        }
        let safe: String = segment
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        out.push(safe);
    }
    // Defense in depth: the result must stay under root.
    debug_assert!(out.components().filter(|c| matches!(c, Component::ParentDir)).count() == 0);
    Ok(out)
}

/// Remove all entries inside `dir` (leaving `dir` itself), creating it if absent.
pub(crate) fn clear_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    } else {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Copy a single path (file or directory) from `src` to `dst`.
pub(crate) fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir_all(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

/// Recursively copy the contents of directory `src` into `dst`.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/store");
        assert!(safe_join(root, "../escape").is_err());
        let ok = safe_join(root, "session/abc-123").unwrap();
        assert_eq!(ok, Path::new("/tmp/store/session/abc-123"));
    }

    #[test]
    fn safe_join_sanitizes_segments() {
        let root = Path::new("/tmp/store");
        let p = safe_join(root, "mem/telegram:42").unwrap();
        assert_eq!(p, Path::new("/tmp/store/mem/telegram_42"));
    }
}
```

- [ ] **Step 4: Write `src/sandbox/state/filesystem.rs` with the impl and tests**

```rust
//! Filesystem-backed `StateStore` (Phase 2; also useful for homelab/dev).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::sandbox::state::{StateStore, clear_dir, copy_dir_all, safe_join};

/// Stores each key as a directory tree under `root`.
pub struct FilesystemStateStore {
    root: PathBuf,
}

impl FilesystemStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl StateStore for FilesystemStateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let src = safe_join(&self.root, key)?;
        if !src.exists() {
            return Ok(false);
        }
        clear_dir(dest)?;
        copy_dir_all(&src, dest)?;
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        if dst.exists() {
            fs::remove_dir_all(&dst)?;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_dir_all(src, &dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn pull_absent_key_returns_false() {
        let root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        assert!(!store.pull("session/missing", dest.path()).await.unwrap());
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_nested_tree() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "alpha");
        write(&src.path().join("sub/b.txt"), "beta");

        let store = FilesystemStateStore::new(root.path().to_path_buf());
        store.push(src.path(), "session/x").await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull("session/x", dest.path()).await.unwrap());
        assert_eq!(fs::read_to_string(dest.path().join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(dest.path().join("sub/b.txt")).unwrap(), "beta");
    }

    #[tokio::test]
    async fn push_overwrites_prior_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());

        let src1 = tempfile::tempdir().unwrap();
        write(&src1.path().join("old.txt"), "old");
        store.push(src1.path(), "k").await.unwrap();

        let src2 = tempfile::tempdir().unwrap();
        write(&src2.path().join("new.txt"), "new");
        store.push(src2.path(), "k").await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        store.pull("k", dest.path()).await.unwrap();
        assert!(!dest.path().join("old.txt").exists());
        assert_eq!(fs::read_to_string(dest.path().join("new.txt")).unwrap(), "new");
    }
}
```

- [ ] **Step 5: Build and test**

Run: `cargo test --lib sandbox::state`
Expected: PASS (5 tests). `cargo build` succeeds (dead-code warnings on `StateStore`/`default_store` are expected until Task 6 wires them).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/sandbox/mod.rs src/sandbox/state/mod.rs src/sandbox/state/filesystem.rs
git commit -m "feat(sandbox): add StateStore trait and FilesystemStateStore"
```

---

### Task 3: `claude_project_slug` + `ClaudeSessionArtifacts`

**Files:**
- Create: `src/sandbox/artifacts.rs`
- Modify: `src/sandbox/mod.rs` (add `pub mod artifacts;`)

- [ ] **Step 1: Register the module**

In `src/sandbox/mod.rs` add near the other module declarations:

```rust
pub mod artifacts;
```

- [ ] **Step 2: Write the failing tests**

Create `src/sandbox/artifacts.rs`:

```rust
//! Maps a Claude session id to its on-disk files and captures/restores them.
//!
//! Capture finds files by session id (slug-independent). Restore writes the
//! transcript under the slug of the *current* cwd, so a worker with a different
//! cwd in a later phase still resumes correctly.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::sandbox::state::{copy_dir_all, copy_path};

/// Slugify a working directory the way Claude Code names its project dir:
/// every non-alphanumeric character becomes `-`.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Capture/restore of Claude session files.
pub struct ClaudeSessionArtifacts;

impl ClaudeSessionArtifacts {
    /// Copy the files making up `session_id` from `claude_home` into `staging`,
    /// laid out as `transcript.jsonl`, `session-env`, and `todos/`.
    /// Returns `false` (capturing nothing) if no transcript is found.
    pub fn capture(claude_home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        let dot = claude_home.join(".claude");
        fs::create_dir_all(staging)?;

        // Transcript: find <session_id>.jsonl under any projects/<slug>/ dir.
        let projects = dot.join("projects");
        let mut transcript: Option<PathBuf> = None;
        if projects.is_dir() {
            for entry in fs::read_dir(&projects)? {
                let candidate = entry?.path().join(format!("{session_id}.jsonl"));
                if candidate.is_file() {
                    transcript = Some(candidate);
                    break;
                }
            }
        }
        let Some(transcript) = transcript else {
            return Ok(false);
        };
        fs::copy(&transcript, staging.join("transcript.jsonl"))?;

        // session-env/<id> (file or dir), if present.
        let env_src = dot.join("session-env").join(session_id);
        if env_src.exists() {
            copy_path(&env_src, &staging.join("session-env"))?;
        }

        // todos/<id>-*.json, if present.
        let todos_src = dot.join("todos");
        if todos_src.is_dir() {
            let prefix = format!("{session_id}-");
            let staged_todos = staging.join("todos");
            for entry in fs::read_dir(&todos_src)? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    fs::create_dir_all(&staged_todos)?;
                    fs::copy(entry.path(), staged_todos.join(&name))?;
                }
            }
        }
        Ok(true)
    }

    /// Restore staged artifacts into `claude_home` so `claude --resume
    /// <session_id>` (run with `cwd`) finds them.
    pub fn restore(claude_home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        let dot = claude_home.join(".claude");

        let transcript = staging.join("transcript.jsonl");
        if transcript.is_file() {
            let proj = dot.join("projects").join(claude_project_slug(cwd));
            fs::create_dir_all(&proj)?;
            fs::copy(&transcript, proj.join(format!("{session_id}.jsonl")))?;
        }

        let env_staged = staging.join("session-env");
        if env_staged.exists() {
            let env_dst = dot.join("session-env").join(session_id);
            if let Some(parent) = env_dst.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_path(&env_staged, &env_dst)?;
        }

        let todos_staged = staging.join("todos");
        if todos_staged.is_dir() {
            copy_dir_all(&todos_staged, &dot.join("todos"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_known_example() {
        let cwd = Path::new("/Users/dcvz/Library/Application Support/cica");
        assert_eq!(
            claude_project_slug(cwd),
            "-Users-dcvz-Library-Application-Support-cica"
        );
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn capture_then_restore_reproduces_files() {
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        // Synthetic source claude_home.
        let home_a = tempfile::tempdir().unwrap();
        let dot_a = home_a.path().join(".claude");
        write(&dot_a.join("projects").join(&slug).join(format!("{id}.jsonl")), "line1\n");
        write(&dot_a.join("session-env").join(id), "ENV=1");
        write(&dot_a.join("todos").join(format!("{id}-agent-{id}.json")), "[]");

        // Capture → staging.
        let staging = tempfile::tempdir().unwrap();
        assert!(ClaudeSessionArtifacts::capture(home_a.path(), id, staging.path()).unwrap());

        // Restore into a fresh home.
        let home_b = tempfile::tempdir().unwrap();
        ClaudeSessionArtifacts::restore(home_b.path(), cwd, id, staging.path()).unwrap();

        let dot_b = home_b.path().join(".claude");
        assert_eq!(
            fs::read_to_string(dot_b.join("projects").join(&slug).join(format!("{id}.jsonl"))).unwrap(),
            "line1\n"
        );
        assert_eq!(fs::read_to_string(dot_b.join("session-env").join(id)).unwrap(), "ENV=1");
        assert_eq!(
            fs::read_to_string(dot_b.join("todos").join(format!("{id}-agent-{id}.json"))).unwrap(),
            "[]"
        );
    }

    #[test]
    fn capture_returns_false_without_transcript() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        assert!(!ClaudeSessionArtifacts::capture(home.path(), "no-such", staging.path()).unwrap());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail, then pass**

Run: `cargo test --lib sandbox::artifacts` — first confirm it compiles and the 3 tests pass. (Write the code in Step 2 already includes the implementation, so this is the green run.)
Expected: PASS (3 tests).

> TDD note: if you prefer strict red-first, comment out the function bodies with `todo!()` and run once to see the failure, then paste the implementations. Either way, end with all 3 passing.

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/mod.rs src/sandbox/artifacts.rs
git commit -m "feat(sandbox): add claude session slug + artifact capture/restore"
```

---

### Task 4: `default_store` factory test

**Files:**
- Modify: `src/sandbox/state/mod.rs` (add tests only)

The factory was written in Task 2; this task adds its tests (kept separate because it depends on Task 1's config types being present).

- [ ] **Step 1: Add the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/sandbox/state/mod.rs`:

```rust
    #[test]
    fn default_store_none_when_unconfigured() {
        let cfg = Config::default();
        assert!(default_store(&cfg).unwrap().is_none());
    }

    #[test]
    fn default_store_some_for_filesystem() {
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-state-test".to_string());
        assert!(default_store(&cfg).unwrap().is_some());
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib sandbox::state::tests::default_store`
Expected: PASS (both). If `Config`/`StoreKind` are not in scope in the test module, ensure `use super::*;` covers them (the factory already imports `crate::config::{Config, StoreKind}` at module level, so `super::*` re-exports them into tests).

- [ ] **Step 3: Commit**

```bash
git add src/sandbox/state/mod.rs
git commit -m "test(sandbox): cover default_store factory selection"
```

---

### Task 5: `HydratingProvider`

**Files:**
- Create: `src/sandbox/hydrating.rs`
- Modify: `src/sandbox/mod.rs` (add `pub mod hydrating;`)

- [ ] **Step 1: Register the module**

In `src/sandbox/mod.rs`:

```rust
pub mod hydrating;
```

- [ ] **Step 2: Write `src/sandbox/hydrating.rs` with the impl and tests**

```rust
//! A `SandboxProvider` decorator that hydrates durable state before a turn
//! and dehydrates it after, via a `StateStore`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::warn;

use crate::config::AiBackend;
use crate::sandbox::artifacts::ClaudeSessionArtifacts;
use crate::sandbox::state::StateStore;
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Wraps an inner provider: pull session + memories → run → capture + push.
pub struct HydratingProvider<P: SandboxProvider> {
    inner: P,
    store: Arc<dyn StateStore>,
    claude_home: PathBuf,
    /// Effective working directory of the agent subprocess (used for the slug).
    cwd: PathBuf,
}

impl<P: SandboxProvider> HydratingProvider<P> {
    pub fn new(inner: P, store: Arc<dyn StateStore>, claude_home: PathBuf, cwd: PathBuf) -> Self {
        Self { inner, store, claude_home, cwd }
    }

    fn memories_dir(&self, channel: &str, user_id: &str) -> PathBuf {
        self.cwd
            .join("users")
            .join(format!("{channel}_{user_id}"))
            .join("memories")
    }

    fn staging(&self) -> PathBuf {
        std::env::temp_dir().join(format!("cica-hydrate-{}", uuid::Uuid::new_v4()))
    }
}

#[async_trait]
impl<P: SandboxProvider> SandboxProvider for HydratingProvider<P> {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let is_claude = matches!(job.backend, AiBackend::Claude);
        let mem_key = format!("mem/{}_{}", job.channel, job.user_id);
        let mem_dir = self.memories_dir(&job.channel, &job.user_id);

        // --- Hydrate ---
        if is_claude {
            if let Some(bid) = &job.resume_session {
                let staging = self.staging();
                if self.store.pull(&format!("session/{bid}"), &staging).await? {
                    ClaudeSessionArtifacts::restore(&self.claude_home, &self.cwd, bid, &staging)?;
                }
                let _ = std::fs::remove_dir_all(&staging);
            }
        } else {
            warn!("HydratingProvider: session hydration unsupported for non-Claude backend; skipping");
        }
        // Memories: pull is authoritative when present; absent = keep local.
        let _ = self.store.pull(&mem_key, &mem_dir).await?;

        // --- Run ---
        let result = self.inner.run_turn(job).await?;

        // --- Dehydrate ---
        if is_claude && !result.backend_session_id.is_empty() {
            let bid = &result.backend_session_id;
            let staging = self.staging();
            if ClaudeSessionArtifacts::capture(&self.claude_home, bid, &staging)? {
                self.store.push(&staging, &format!("session/{bid}")).await?;
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        if mem_dir.exists() {
            self.store.push(&mem_dir, &mem_key).await?;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::sandbox::state::FilesystemStateStore;

    /// Inner provider that records the job and returns a fixed session id.
    struct StubProvider {
        session_id: String,
        seen: Mutex<Option<TurnJob>>,
    }

    #[async_trait]
    impl SandboxProvider for StubProvider {
        async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
            *self.seen.lock().unwrap() = Some(job);
            Ok(TurnResult {
                response: "ok".into(),
                backend_session_id: self.session_id.clone(),
                cost_usd: None,
                duration_ms: None,
            })
        }
    }

    fn job(resume: Option<&str>) -> TurnJob {
        TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: resume.map(|s| s.to_string()),
            cwd: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn dehydrate_captures_and_pushes_result_session() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // Simulate the transcript the (stub) turn "produced".
        let id = "sess-new";
        let slug = crate::sandbox::artifacts::claude_project_slug(base.path());
        write(
            &claude_home.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl")),
            "turn1\n",
        );

        let inner = StubProvider { session_id: id.into(), seen: Mutex::new(None) };
        let hp = HydratingProvider::new(inner, store.clone(), claude_home.path().to_path_buf(), base.path().to_path_buf());
        hp.run_turn(job(None)).await.unwrap();

        // It should now be retrievable from the store.
        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull(&format!("session/{id}"), dest.path()).await.unwrap());
        assert_eq!(std::fs::read_to_string(dest.path().join("transcript.jsonl")).unwrap(), "turn1\n");
    }

    #[tokio::test]
    async fn hydrate_restores_resumed_session() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // Pre-stage a stored session under session/<id>.
        let id = "sess-old";
        let staged = tempfile::tempdir().unwrap();
        write(&staged.path().join("transcript.jsonl"), "history\n");
        store.push(staged.path(), &format!("session/{id}")).await.unwrap();

        let inner = StubProvider { session_id: id.into(), seen: Mutex::new(None) };
        let hp = HydratingProvider::new(inner, store, claude_home.path().to_path_buf(), base.path().to_path_buf());
        hp.run_turn(job(Some(id))).await.unwrap();

        // The transcript must have been restored under slug(base).
        let slug = crate::sandbox::artifacts::claude_project_slug(base.path());
        let restored = claude_home.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl"));
        assert_eq!(std::fs::read_to_string(restored).unwrap(), "history\n");
    }

    #[tokio::test]
    async fn memories_round_trip() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // The (stub) turn "writes" a memory file into the local memories dir.
        let mem_dir = base.path().join("users").join("telegram_1").join("memories");
        write(&mem_dir.join("note.md"), "remember this");

        let inner = StubProvider { session_id: String::new(), seen: Mutex::new(None) };
        let hp = HydratingProvider::new(inner, store.clone(), claude_home.path().to_path_buf(), base.path().to_path_buf());
        hp.run_turn(job(None)).await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull("mem/telegram_1", dest.path()).await.unwrap());
        assert_eq!(std::fs::read_to_string(dest.path().join("note.md")).unwrap(), "remember this");
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib sandbox::hydrating`
Expected: PASS (3 tests). `cargo build` succeeds.

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/mod.rs src/sandbox/hydrating.rs
git commit -m "feat(sandbox): add HydratingProvider decorator"
```

---

### Task 6: Wire `default_provider`, lint, and document the manual round-trip

**Files:**
- Modify: `src/sandbox/mod.rs`

- [ ] **Step 1: Extend `default_provider` to compose the decorator**

Current (Phase 1) body:
```rust
pub fn default_provider(_config: &Config) -> Box<dyn SandboxProvider> {
    Box::new(LocalProcessProvider::new())
}
```
Replace with:
```rust
pub fn default_provider(config: &Config) -> Box<dyn SandboxProvider> {
    let local = LocalProcessProvider::new();
    match state::default_store(config) {
        Ok(Some(store)) => match crate::config::paths() {
            Ok(paths) => Box::new(hydrating::HydratingProvider::new(
                local,
                store,
                paths.claude_home,
                paths.base,
            )),
            Err(e) => {
                tracing::warn!("state store configured but paths unavailable ({e}); running without hydration");
                Box::new(LocalProcessProvider::new())
            }
        },
        Ok(None) => Box::new(local),
        Err(e) => {
            tracing::warn!("failed to build state store ({e}); running without hydration");
            Box::new(local)
        }
    }
}
```
(Ensure `LocalProcessProvider::new()` is constructed twice only in the fallback arms; the first `local` is moved into the `HydratingProvider` in the success arm, so the fallback arms build a fresh one — as written above.)

- [ ] **Step 2: Build and run the full suite**

Run: `cargo build && cargo test`
Expected: SUCCESS; all tests pass (Phase 1's 26 + the new state/artifacts/hydrating/config tests).

- [ ] **Step 3: Lint gate and dead-code sweep**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS. The `StateStore`/`default_store`/`FilesystemStateStore`/`ClaudeSessionArtifacts`/`HydratingProvider` items are now all reachable from `default_provider`, so no `dead_code` warnings should remain. If clippy flags a genuinely-unused item, prefer wiring/removing it over a blanket `#[allow]`; only add `#[allow(dead_code)]` (with a one-line reason) for a forward-looking field, and report it.

- [ ] **Step 4: Format**

Run: `cargo fmt` then `cargo fmt --check`
Expected: clean.

- [ ] **Step 5: Manual round-trip resume test (document — cannot run in CI)**

The real `claude --resume` round-trip needs the `claude` CLI + credentials and a configured cica, so it is a manual integration test, not a unit test. Record these steps in the PR description / release notes and run them in a configured environment:

1. Set `[deployment] store = "filesystem"` in `config.toml`.
2. Send a message; note the assistant replies and a `session/<id>` dir appears under `internal/state-store/` and `internal/state-store/mem/<channel>_<user>/`.
3. Delete `internal/claude-home/.claude/projects/*` (the local transcript) to force reliance on the store.
4. Send a follow-up message in the same conversation; confirm the assistant resumes context (the `HydratingProvider` restored the transcript from the store before `--resume`).
5. Confirm a memory written in step 2 is still searchable after step 4.

If resume fails after step 3, expand `ClaudeSessionArtifacts::capture`/`restore` to include any additional files Claude needs (e.g. a `.claude.json` project entry) and re-run.

- [ ] **Step 6: Commit**

```bash
git add src/sandbox/mod.rs
git commit -m "feat(sandbox): compose HydratingProvider when a state store is configured"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- `StateStore` trait + `FilesystemStateStore` (S3/GCS deferred) → Tasks 2, 4.
- Key-by-`session_id`/backend-id, decoupled from slug; restore under current-cwd slug → Task 3 (`restore` uses `claude_project_slug(cwd)`), Task 5 (`session/<backend_session_id>` keys).
- Capture set = transcript + session-env + todos; empirically validated → Task 3 (capture/restore) + Task 6 Step 5 (manual resume round-trip, with explicit "expand if it fails").
- Memories sync (pull before / push after; router still re-indexes) → Task 5; re-index is unchanged existing behavior in `channels/mod.rs`.
- `HydratingProvider` decorator; composed only when store configured; bare local otherwise → Tasks 5, 6.
- Default behavior unchanged with no store → Task 6 (`Ok(None)` arm).
- claude-only with cursor seam → Task 5 (`is_claude` gate + warn).
- Error handling (absent pull = fresh; push failure = turn error) → Task 5 (pull bool ignored for memories/absent session; `?` propagates push errors), Task 2 (pull returns false on absent).
- Distribution neutral; no new runtime deps → only `tempfile` added under `[dev-dependencies]` (Task 2).

**Placeholder scan:** No "TBD"/"handle errors appropriately"/"similar to Task N". Every code step contains complete code. The only `todo!()` mention is an optional strict-TDD aid in Task 3 Step 3, not required.

**Type consistency:** `StateStore::{pull(&self, key:&str, dest:&Path)->Result<bool>, push(&self, src:&Path, key:&str)->Result<()>}` is identical across `state/mod.rs`, `filesystem.rs`, `hydrating.rs`, and the tests. `ClaudeSessionArtifacts::{capture(claude_home,session_id,staging)->Result<bool>, restore(claude_home,cwd,session_id,staging)->Result<()>}` and `claude_project_slug(cwd)->String` are used consistently in Tasks 3 and 5. `HydratingProvider::new(inner, store, claude_home, cwd)` matches its call in Task 6. `default_store(&Config)->Result<Option<Arc<dyn StateStore>>>` matches Tasks 2, 4, 6. Store keys `session/<backend_session_id>` and `mem/<channel>_<user_id>` are consistent between `hydrating.rs` and its tests.

## Next phase (separate plan)

Phase 3: `cica worker` subcommand; container/Fargate/Cloud Run launchers (feature-gated); real S3/GCS `StateStore` impls; worker `Dockerfile` + deployment contract; result-return-from-worker; cwd canonicalization; feature-gated cloud release artifacts + `--all-features` CI.
