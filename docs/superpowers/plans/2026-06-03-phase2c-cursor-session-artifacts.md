# Phase 2c: Cursor Session Artifacts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make session capture/restore work for the Cursor backend (the production deployment) by adding a `SessionArtifacts` trait, a `CursorSessionArtifacts` impl for Cursor's per-session SQLite store, and backend dispatch in `HydratingProvider` — so the worker fleet persists/restores Cursor sessions, not just Claude.

**Architecture:** A `SessionArtifacts` trait abstracts backend-specific capture/restore. `ClaudeSessionArtifacts` implements it by delegating to its existing inherent fns (behavior unchanged). `CursorSessionArtifacts` captures/restores `.cursor/chats/<workspace_hash>/<session_id>/store.db{,-wal,-shm}`, recording the workspace-hash dir and replaying it (the hash is `md5(realpath(cwd))`, so a consistent worker cwd guarantees portability — record-replay needs no md5 dependency). `HydratingProvider` selects the impl and the backend's HOME (`claude_home`/`cursor_home`) by `job.backend`, dropping the old non-Claude skip.

**Tech Stack:** Rust 2024, `std::fs`, `tokio`, `async-trait`, `anyhow`, `uuid` (existing). `tempfile` (existing dev-dep) for tests. No new dependencies.

---

## Why this is safe and incremental

Additive, behind the existing opt-in `[deployment]` config. Claude's artifact logic is untouched (the trait delegates to its inherent fns). The only behavior change is intentional: the Cursor backend now hydrates/captures sessions instead of being skipped. No new dependencies.

## Background facts (verified against the code + live deployment)

- `src/sandbox/artifacts.rs`: `ClaudeSessionArtifacts` has inherent fns `capture(claude_home: &Path, session_id: &str, staging: &Path) -> Result<bool>` and `restore(claude_home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()>`, plus free fn `claude_project_slug(cwd: &Path) -> String`. It uses `crate::sandbox::state::{clear_dir, copy_dir_all, copy_path}`.
- `src/sandbox/hydrating.rs`: `HydratingProvider<P>` has fields `inner, store, claude_home, cwd` and `new(inner, store, claude_home, cwd)`. `run_turn` gates on `is_claude = matches!(job.backend, AiBackend::Claude)`: hydrate (pull `session/<bid>` → `ClaudeSessionArtifacts::restore`), memories pull, run, dehydrate (`ClaudeSessionArtifacts::capture` → push `session/<bid>`), memories push. Uses `tracing::warn`.
- `crate::config::AiBackend` has exactly `Claude` and `Cursor`.
- `crate::config::paths()` → `Paths` with `claude_home: PathBuf` and `cursor_home: PathBuf` (`internal/cursor-home`).
- Cursor session on disk: `cursor_home/.cursor/chats/<workspace_hash>/<session_id>/store.db` (+ `store.db-wal`, `store.db-shm`). `<workspace_hash> = md5(realpath(cwd))` (verified: `md5("/data/cica") = 5c64d42749f92f28359bff54fe4cb4bc`). `session_id` is what cica tracks and passes to `cursor --resume`.
- `HydratingProvider::new` callers: `src/sandbox/mod.rs` (`try_default_provider`, Local arm) and `src/cmd/worker.rs`.

## File structure

- Modify `src/sandbox/artifacts.rs` — add `SessionArtifacts` trait; `impl SessionArtifacts for ClaudeSessionArtifacts` (delegating); add `CursorSessionArtifacts` + its `impl SessionArtifacts`.
- Modify `src/sandbox/hydrating.rs` — add `cursor_home` field + ctor arg; dispatch `(artifacts, home)` by `job.backend`; remove the `is_claude` skip.
- Modify `src/sandbox/mod.rs` + `src/cmd/worker.rs` — pass `paths.cursor_home` to `HydratingProvider::new`.

---

### Task 1: `SessionArtifacts` trait + Claude impl (delegating)

**Files:**
- Modify: `src/sandbox/artifacts.rs`

- [ ] **Step 1: Add the trait and the Claude impl**

At the top of `src/sandbox/artifacts.rs`, after the existing `use` lines, add the trait:

```rust
/// Backend-specific capture/restore of a session's on-disk state.
///
/// `home` is the backend's HOME dir (claude_home or cursor_home). `capture`
/// copies the files making up `session_id` into `staging` (returns false if the
/// session isn't found); `restore` reinstates them under `home` so a resume run
/// with `cwd` finds them.
pub trait SessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool>;
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()>;
}
```

After the existing `impl ClaudeSessionArtifacts { ... }` block, add the trait impl that delegates to the inherent fns (the 3-/4-arg calls resolve to the inherent fns, not the trait methods — no recursion):

```rust
impl SessionArtifacts for ClaudeSessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        ClaudeSessionArtifacts::capture(home, session_id, staging)
    }
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        ClaudeSessionArtifacts::restore(home, cwd, session_id, staging)
    }
}
```

- [ ] **Step 2: Add a trait-dispatch test**

In the `#[cfg(test)] mod tests` block of `src/sandbox/artifacts.rs`, add (it reuses the existing `write` helper and `claude_project_slug`):

```rust
    #[test]
    fn claude_via_trait_round_trips() {
        let artifacts: &dyn SessionArtifacts = &ClaudeSessionArtifacts;
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        let home_a = tempfile::tempdir().unwrap();
        write(
            &home_a.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl")),
            "line1\n",
        );
        let staging = tempfile::tempdir().unwrap();
        assert!(artifacts.capture(home_a.path(), id, staging.path()).unwrap());

        let home_b = tempfile::tempdir().unwrap();
        artifacts.restore(home_b.path(), cwd, id, staging.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(
                home_b.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl"))
            ).unwrap(),
            "line1\n"
        );
    }
```

- [ ] **Step 3: Build + test**

Run: `cargo test sandbox::artifacts`
Expected: all existing Claude artifact tests still pass + the new `claude_via_trait_round_trips`. `cargo build` succeeds (a `dead_code` warning on the trait until Task 3 wires it is fine; do NOT add `#[allow]`).

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/artifacts.rs
git commit -m "feat(sandbox): add SessionArtifacts trait; Claude impl delegates"
```
End every commit with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: `CursorSessionArtifacts`

**Files:**
- Modify: `src/sandbox/artifacts.rs`

- [ ] **Step 1: Add the type and impl**

In `src/sandbox/artifacts.rs`, ensure `use std::fs;` is present (add if missing — Claude code already uses `fs`). Add:

```rust
/// Capture/restore of Cursor session state.
///
/// A Cursor session lives at `cursor_home/.cursor/chats/<workspace_hash>/<id>/`
/// as SQLite files (`store.db` + `-wal` + `-shm`). The workspace hash is
/// `md5(realpath(cwd))`; we record the hash dir at capture and replay it at
/// restore — correct as long as all workers share a resolved cwd (the fleet
/// requirement), and resilient to Cursor changing its hashing.
pub struct CursorSessionArtifacts;

const CURSOR_DB_FILES: [&str; 3] = ["store.db", "store.db-wal", "store.db-shm"];

impl SessionArtifacts for CursorSessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        let chats = home.join(".cursor").join("chats");
        if !chats.is_dir() {
            return Ok(false);
        }
        // Find <workspace_hash>/<session_id>/ under chats (hash-independent).
        for entry in fs::read_dir(&chats)? {
            let ws = entry?;
            let session_dir = ws.path().join(session_id);
            if session_dir.is_dir() {
                // Stage as staging/<workspace_hash>/<files>, recording the hash.
                let dest = staging.join(ws.file_name());
                fs::create_dir_all(&dest)?;
                for f in CURSOR_DB_FILES {
                    let src = session_dir.join(f);
                    if src.is_file() {
                        fs::copy(&src, dest.join(f))?;
                    }
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn restore(&self, home: &Path, _cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        // The single subdir in staging is the recorded workspace hash.
        let Some(hash) = fs::read_dir(staging)?
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .map(|e| e.file_name())
        else {
            return Ok(()); // nothing staged
        };
        let staged_dir = staging.join(&hash);
        let dest = home.join(".cursor").join("chats").join(&hash).join(session_id);
        fs::create_dir_all(&dest)?;
        for f in CURSOR_DB_FILES {
            let src = staged_dir.join(f);
            if src.is_file() {
                fs::copy(&src, dest.join(f))?;
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Add tests**

In the `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn cursor_capture_then_restore_reproduces_session_db() {
        let id = "6cd64aba-d369-4444-b2f9-acda76abdf3f";
        let hash = "5c64d42749f92f28359bff54fe4cb4bc";

        // Synthetic source cursor-home with a session's SQLite files.
        let home_a = tempfile::tempdir().unwrap();
        let session_dir = home_a.path().join(".cursor").join("chats").join(hash).join(id);
        write(&session_dir.join("store.db"), "DB");
        write(&session_dir.join("store.db-wal"), "WAL");
        write(&session_dir.join("store.db-shm"), "SHM");

        let artifacts = CursorSessionArtifacts;
        let staging = tempfile::tempdir().unwrap();
        assert!(artifacts.capture(home_a.path(), id, staging.path()).unwrap());

        // Restore into a fresh home; cwd is ignored for cursor (record-replay).
        let home_b = tempfile::tempdir().unwrap();
        artifacts.restore(home_b.path(), Path::new("/whatever"), id, staging.path()).unwrap();

        let dest = home_b.path().join(".cursor").join("chats").join(hash).join(id);
        assert_eq!(std::fs::read_to_string(dest.join("store.db")).unwrap(), "DB");
        assert_eq!(std::fs::read_to_string(dest.join("store.db-wal")).unwrap(), "WAL");
        assert_eq!(std::fs::read_to_string(dest.join("store.db-shm")).unwrap(), "SHM");
    }

    #[test]
    fn cursor_capture_returns_false_when_absent() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let artifacts = CursorSessionArtifacts;
        assert!(!artifacts.capture(home.path(), "no-such", staging.path()).unwrap());
    }

    #[test]
    fn cursor_capture_tolerates_missing_wal_shm() {
        // A checkpointed session may have only store.db.
        let id = "sess-1";
        let hash = "abc123";
        let home_a = tempfile::tempdir().unwrap();
        write(&home_a.path().join(".cursor").join("chats").join(hash).join(id).join("store.db"), "DB");

        let artifacts = CursorSessionArtifacts;
        let staging = tempfile::tempdir().unwrap();
        assert!(artifacts.capture(home_a.path(), id, staging.path()).unwrap());

        let home_b = tempfile::tempdir().unwrap();
        artifacts.restore(home_b.path(), Path::new("/x"), id, staging.path()).unwrap();
        let dest = home_b.path().join(".cursor").join("chats").join(hash).join(id);
        assert_eq!(std::fs::read_to_string(dest.join("store.db")).unwrap(), "DB");
        assert!(!dest.join("store.db-wal").exists());
    }
```

- [ ] **Step 3: Build + test**

Run: `cargo test sandbox::artifacts`
Expected: 3 new cursor tests pass + all prior pass. `cargo build` succeeds (dead-code warning on `CursorSessionArtifacts` until Task 3; no `#[allow]`).

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/artifacts.rs
git commit -m "feat(sandbox): add CursorSessionArtifacts (SQLite session capture/restore)"
```

---

### Task 3: Dispatch by backend in `HydratingProvider`

**Files:**
- Modify: `src/sandbox/hydrating.rs`

- [ ] **Step 1: Add `cursor_home`, update imports, dispatch by backend**

Update imports near the top of `src/sandbox/hydrating.rs`:
- Add `use std::path::Path;` (used by the dispatch tuple) if not present — `PathBuf` is already imported; add `Path` to that line: `use std::path::{Path, PathBuf};`.
- Add the artifacts types/trait: `use crate::sandbox::artifacts::{ClaudeSessionArtifacts, CursorSessionArtifacts, SessionArtifacts};` (replacing the existing `use crate::sandbox::artifacts::ClaudeSessionArtifacts;`).
- The `tracing::warn` import becomes unused after removing the skip — remove `use tracing::warn;`.

Add the `cursor_home` field and ctor arg:

```rust
pub struct HydratingProvider<P: SandboxProvider> {
    inner: P,
    store: Arc<dyn StateStore>,
    claude_home: PathBuf,
    cursor_home: PathBuf,
    /// Effective working directory of the agent subprocess (used for the slug/hash).
    cwd: PathBuf,
}

impl<P: SandboxProvider> HydratingProvider<P> {
    pub fn new(
        inner: P,
        store: Arc<dyn StateStore>,
        claude_home: PathBuf,
        cursor_home: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        Self { inner, store, claude_home, cursor_home, cwd }
    }
```

(Leave `memories_dir` and `staging` unchanged.)

- [ ] **Step 2: Rewrite `run_turn` to dispatch by backend**

Replace the body of `run_turn` with (note: no more `is_claude` skip — both backends hydrate/capture; memories unchanged):

```rust
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let mem_key = format!("mem/{}_{}", job.channel, job.user_id);
        let mem_dir = self.memories_dir(&job.channel, &job.user_id);

        // Select the backend's artifact handler and HOME dir.
        let (artifacts, home): (Box<dyn SessionArtifacts>, &Path) = match job.backend {
            AiBackend::Claude => (Box::new(ClaudeSessionArtifacts), self.claude_home.as_path()),
            AiBackend::Cursor => (Box::new(CursorSessionArtifacts), self.cursor_home.as_path()),
        };

        // --- Hydrate ---
        if let Some(bid) = &job.resume_session {
            let staging = self.staging();
            if self.store.pull(&format!("session/{bid}"), &staging).await? {
                artifacts.restore(home, &self.cwd, bid, &staging)?;
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        // Memories: pull is authoritative when present; absent = keep local.
        let _ = self.store.pull(&mem_key, &mem_dir).await?;

        // --- Run ---
        let result = self.inner.run_turn(job).await?;

        // --- Dehydrate ---
        if !result.backend_session_id.is_empty() {
            let bid = &result.backend_session_id;
            let staging = self.staging();
            if artifacts.capture(home, bid, &staging)? {
                self.store.push(&staging, &format!("session/{bid}")).await?;
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        if mem_dir.exists() {
            self.store.push(&mem_dir, &mem_key).await?;
        }

        Ok(result)
    }
```

- [ ] **Step 3: Update existing tests' constructor calls + add a Cursor dispatch test**

In the `#[cfg(test)] mod tests` of `src/sandbox/hydrating.rs`, every `HydratingProvider::new(inner, store..., claude_home.path().to_path_buf(), base.path().to_path_buf())` call now needs a `cursor_home` argument. The simplest hermetic change: add a `cursor_home` tempdir in each test and pass it. For the three existing tests (`dehydrate_captures_and_pushes_result_session`, `hydrate_restores_resumed_session`, `memories_round_trip`), insert before constructing the provider:

```rust
        let cursor_home = tempfile::tempdir().unwrap();
```

and change each `HydratingProvider::new(...)` to:

```rust
        let hp = HydratingProvider::new(
            inner,
            store.clone(), // or `store` where the test doesn't reuse it afterward
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
```

(Keep each test's existing `store` vs `store.clone()` usage as it was — only insert the `cursor_home` argument in the 4th position.)

Then add a new test proving a Cursor job routes to Cursor artifacts and pushes the session db to the store. The `StubProvider` returns a fixed `backend_session_id`; we pre-seed a synthetic cursor session under `cursor_home` and assert it lands in the store:

```rust
    #[tokio::test]
    async fn cursor_job_captures_session_to_store() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let cursor_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // The (stub) cursor turn "produced" a session db on local disk.
        let id = "cursor-sess-1";
        let hash = "deadbeef";
        write(
            &cursor_home.path().join(".cursor").join("chats").join(hash).join(id).join("store.db"),
            "CURSORDB",
        );

        let inner = StubProvider { session_id: id.into(), seen: Mutex::new(None) };
        let hp = HydratingProvider::new(
            inner,
            store.clone(),
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
        // job() defaults to Claude; override the backend to Cursor.
        let mut j = job(None);
        j.backend = crate::config::AiBackend::Cursor;
        hp.run_turn(j).await.unwrap();

        // The cursor session db must now be retrievable from the store.
        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull(&format!("session/{id}"), dest.path()).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(dest.path().join(hash).join("store.db")).unwrap(),
            "CURSORDB"
        );
    }
```

> The existing `job(resume)` helper builds a `TurnJob` with `backend: AiBackend::Claude`; this test mutates `.backend` to `Cursor`. `write` and `StubProvider` already exist in the test module.

- [ ] **Step 4: Build + test**

Run: `cargo test sandbox::hydrating`
Expected: the 3 updated tests + the new `cursor_job_captures_session_to_store` pass. `cargo build` will FAIL to compile until Task 4 updates the two `HydratingProvider::new` callers — that's expected; proceed to Task 4 before the full build. (Running just the hydrating unit tests compiles the test target, which may also surface the caller errors; if so, do Step 5/commit after Task 4. To keep this task self-contained, you may do Task 4's caller edits now and commit them together — see note.)

> **Sequencing note:** adding the `cursor_home` ctor arg breaks the two callers in `mod.rs` and `cmd/worker.rs`. If `cargo build`/`cargo test` can't run cleanly here, do Task 4's two one-line edits first, then run the suite, then commit Task 3 + Task 4 together with the message below. Either order is fine; the goal is a green `cargo test` at the commit.

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/hydrating.rs
git commit -m "feat(sandbox): dispatch session artifacts by backend in HydratingProvider"
```

---

### Task 4: Update `HydratingProvider::new` callers

**Files:**
- Modify: `src/sandbox/mod.rs`
- Modify: `src/cmd/worker.rs`

- [ ] **Step 1: `try_default_provider` (Local arm) in `src/sandbox/mod.rs`**

Change the `HydratingProvider::new(...)` call (Local + store-present arm) to pass `paths.cursor_home`:

```rust
                    Ok(Box::new(hydrating::HydratingProvider::new(
                        local,
                        store,
                        paths.claude_home,
                        paths.cursor_home,
                        paths.base,
                    )))
```

- [ ] **Step 2: `src/cmd/worker.rs`**

Change the engine construction to pass `paths.cursor_home`:

```rust
    let engine = HydratingProvider::new(
        LocalProcessProvider::new(),
        store.clone(),
        paths.claude_home,
        paths.cursor_home,
        paths.base,
    );
```

- [ ] **Step 3: Build + full test**

Run: `cargo build && cargo test`
Expected: SUCCESS; all tests pass (Phase 1/2/3a + the new artifacts/hydrating tests).

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/mod.rs src/cmd/worker.rs
git commit -m "feat(sandbox): pass cursor_home into HydratingProvider"
```

> If you combined Task 3 + Task 4 per the sequencing note, skip this commit (already included).

---

### Task 5: Lint, fmt, manual-validation doc

**Files:**
- Possibly modify: `src/sandbox/hydrating.rs` (imports only)
- Modify: `docs/superpowers/plans/2026-06-03-phase2c-cursor-session-artifacts.md`

- [ ] **Step 1: Clippy gate**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS. `CursorSessionArtifacts`, the trait, and `cursor_home` are all reachable now. Remove any unused import clippy flags (e.g. the removed `use tracing::warn;`, or an unused `Path` if the dispatch tuple ended up not needing it). Do NOT blanket-`#[allow(dead_code)]`.

- [ ] **Step 2: Format**

Run: `cargo fmt` then `cargo fmt --check` (expect clean).

- [ ] **Step 3: Full test run**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 4: Append manual validation to this plan**

Append to the END of `docs/superpowers/plans/2026-06-03-phase2c-cursor-session-artifacts.md`:

```markdown

## Manual validation — clears the 3a gate for the Cursor backend (run in the configured env)

With `[deployment] provider = "subprocess"` + `store = "filesystem"` and the Cursor backend:
1. Send a message. Confirm a `session/<id>/` dir now appears in `<base>/internal/state-store/` (it didn't before this phase), containing `<workspace_hash>/store.db`.
2. **Force the store path:** `rm -rf <base>/internal/cursor-home/.cursor/chats/` (wipe Cursor's local sessions).
3. Send a follow-up in the same conversation. If it resumes context, the worker restored the session db from the store (the real cross-machine behavior the same-box test masked). If it forgets, Cursor also needs cloud state for `--resume` — capture is still correct, but note the worker must reach Cursor's API (it does).
4. Confirm a memory written in step 1 persists.
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(sandbox): fmt + clippy for cursor artifacts; document manual validation"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- `SessionArtifacts` trait + Claude delegates → Task 1.
- `CursorSessionArtifacts` capture/restore of `.cursor/chats/<hash>/<id>/store.db{,-wal,-shm}`, record-replay hash, tolerates missing wal/shm → Task 2.
- Backend dispatch in `HydratingProvider` (impl + correct HOME), remove non-Claude skip, add `cursor_home` → Task 3.
- Callers pass `cursor_home` → Task 4.
- md5-pure-cwd guarantee / record-replay mechanism → documented in spec + Task 2 doc comment; no md5 dependency added.
- Memory round-trip unchanged for both backends → Task 3 keeps memories pull/push backend-agnostic.
- Clippy/fmt + manual wipe-and-resume validation → Task 5.
- 3b consistent-cwd requirement → recorded in the spec (deployment contract); not code in this phase.

**Placeholder scan:** No "TBD"/"handle errors appropriately"/"similar to Task N". Every code step has complete code. The sequencing note in Task 3 is explicit guidance, not a placeholder.

**Type consistency:** `SessionArtifacts::{capture(&self, home, session_id, staging)->Result<bool>, restore(&self, home, cwd, session_id, staging)->Result<()>}` is identical across Tasks 1–3. `HydratingProvider::new(inner, store, claude_home, cursor_home, cwd)` matches Tasks 3 (def), 4 (both callers), and the updated tests. `CursorSessionArtifacts` staging layout (`staging/<workspace_hash>/store.db*`) is consistent between capture (writes it), restore (reads it), and the dispatch test (asserts `dest.join(hash).join("store.db")`). `CURSOR_DB_FILES` used in both capture and restore.

## Next (after this merges)

Run the manual validation to clear the 3a gate for Cursor. Then Phase 3b: containerize the worker (pin canonical cwd = `/data/cica`), `ContainerProvider` + AWS Fargate launcher, feature-gated `S3StateStore`, pass `cursor.api_key` to the worker, network result-return, deployment-contract doc.

## Manual validation — clears the 3a gate for the Cursor backend (run in the configured env)

With `[deployment] provider = "subprocess"` + `store = "filesystem"` and the Cursor backend:
1. Send a message. Confirm a `session/<id>/` dir now appears in `<base>/internal/state-store/` (it didn't before this phase), containing `<workspace_hash>/store.db`.
2. **Force the store path:** `rm -rf <base>/internal/cursor-home/.cursor/chats/` (wipe Cursor's local sessions).
3. Send a follow-up in the same conversation. If it resumes context, the worker restored the session db from the store (the real cross-machine behavior the same-box test masked). If it forgets, Cursor also needs cloud state for `--resume` — capture is still correct, but note the worker must reach Cursor's API (it does).
4. Confirm a memory written in step 1 persists.
