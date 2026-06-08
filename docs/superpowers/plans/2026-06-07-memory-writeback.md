# Per-User Memory Write-Back (River E2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Revive cica's per-user memory loop in the distributed deployment — memories written on a worker persist to S3 and become searchable from the router next turn — while leaving single-box mode byte-for-byte unchanged.

**Architecture:** Three changes in cica, each with its global/IO-heavy core wrapped around a small pure seam that gets the unit test. (1) The system prompt emits a `{MEMORIES_DIR}` token that `LocalProcessProvider` substitutes to the local per-user memories path at run time. (2) `reindex_user_memories` pulls `mem/...` from the state store before reindexing, gated on a configured store. (3) The `## Memories` prompt guidance is rewritten with a personal-vs-org routing rule.

**Tech Stack:** Rust 2024 (cica bin crate). Reuses `memory::MemoryIndex` (fastembed BGE-small + sqlite-vec), the `StateStore` trait (`pull`/`push`), `LocalProcessProvider`/`HydratingProvider`. Tests: `cargo test --bin cica`.

**Branch:** `feat/memory-writeback` (already created off `main`).

---

## File Structure

- `src/memory.rs` — **modify**: add `pub const MEMORIES_DIR_TOKEN`. Already owns `memories_dir(channel, user_id)` (the shared path helper; derives from `config::paths().base`, which equals the worker's `HydratingProvider` cwd in prod).
- `src/sandbox/local.rs` — **modify**: add a pure `substitute_token` helper + wire it into `job_to_query_options`; ensure the memories dir exists in `run_turn`. This is Change #1.
- `src/channels/mod.rs` — **modify**: add a pure `pull_memories_with_store` helper, make `reindex_user_memories` async + pull-before-reindex, update the single call site to `.await`. This is Change #2.
- `src/onboarding.rs` — **modify**: rewrite the `## Memories` guidance block to emit `{MEMORIES_DIR}` + the routing rule. This is Change #3.

---

## Task 1: `{MEMORIES_DIR}` token + worker-side substitution (Change #1)

**Files:**
- Modify: `src/memory.rs` (add the token const near `memories_dir`, ~line 60)
- Modify: `src/sandbox/local.rs` (substitution helper + wiring + dir creation)
- Test: `src/sandbox/local.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the shared token constant**

In `src/memory.rs`, just above `pub fn memories_dir` (currently line 61), add:

```rust
/// Placeholder emitted into the system prompt by the router; substituted to the
/// local per-user memories path by `LocalProcessProvider` in the process that
/// actually runs the agent (the worker in cloud mode, the box itself single-box).
pub const MEMORIES_DIR_TOKEN: &str = "{MEMORIES_DIR}";
```

- [ ] **Step 2: Write the failing test for the pure substitution helper**

In `src/sandbox/local.rs`, inside the existing `#[cfg(test)] mod tests` block (after `provider_is_constructible_and_object_safe`, before the closing `}`), add:

```rust
use std::path::Path;

#[test]
fn substitutes_memories_token_when_present() {
    let out = substitute_token(Some("save to {MEMORIES_DIR}/x.md please"), Path::new("/data/cica/users/telegram_1/memories"));
    assert_eq!(out.as_deref(), Some("save to /data/cica/users/telegram_1/memories/x.md please"));
}

#[test]
fn leaves_prompt_unchanged_when_token_absent() {
    let out = substitute_token(Some("no token here"), Path::new("/m"));
    assert_eq!(out.as_deref(), Some("no token here"));
}

#[test]
fn none_prompt_stays_none() {
    let out = substitute_token(None, Path::new("/m"));
    assert_eq!(out, None);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --bin cica substitutes_memories_token_when_present`
Expected: FAIL — `cannot find function substitute_token in this scope`.

- [ ] **Step 4: Write the substitution helper + wire it in**

In `src/sandbox/local.rs`, add the helper above `job_to_query_options` (after the `impl SandboxProvider` block, ~line 27):

```rust
/// Resolve `{MEMORIES_DIR}` in the system prompt to the given local memories
/// path. Token absent → prompt returned unchanged; `None` prompt → `None`.
fn substitute_token(system_prompt: Option<&str>, memories_dir: &std::path::Path) -> Option<String> {
    let sp = system_prompt?;
    Some(sp.replace(crate::memory::MEMORIES_DIR_TOKEN, &memories_dir.to_string_lossy()))
}
```

Then change `job_to_query_options` to resolve the token. Replace the current body:

```rust
fn job_to_query_options(job: &TurnJob) -> backends::QueryOptions {
    backends::QueryOptions {
        system_prompt: job.system_prompt.clone(),
        resume_session: job.resume_session.clone(),
        cwd: job.cwd.clone(),
        skip_permissions: job.skip_permissions,
    }
}
```

with:

```rust
fn job_to_query_options(job: &TurnJob) -> backends::QueryOptions {
    // The agent runs in *this* process, so the local per-user memories path is
    // the one it can write to and that HydratingProvider later captures. Token
    // unresolvable (path lookup fails) → leave the prompt as-is, harmless.
    let system_prompt = match crate::memory::memories_dir(&job.channel, &job.user_id) {
        Ok(dir) => substitute_token(job.system_prompt.as_deref(), &dir),
        Err(_) => job.system_prompt.clone(),
    };
    backends::QueryOptions {
        system_prompt,
        resume_session: job.resume_session.clone(),
        cwd: job.cwd.clone(),
        skip_permissions: job.skip_permissions,
    }
}
```

- [ ] **Step 5: Ensure the memories dir exists before the turn**

In `src/sandbox/local.rs`, update `run_turn` (currently lines 21-25) to create the dir best-effort:

```rust
async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
    // Make sure the per-user memories dir exists so the agent can write into it.
    if let Ok(dir) = crate::memory::memories_dir(&job.channel, &job.user_id) {
        let _ = std::fs::create_dir_all(&dir);
    }
    let options = job_to_query_options(&job);
    let qr = backends::query_with_options(&job.prompt, options).await?;
    Ok(turn_result_from_query(qr))
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --bin cica --lib 2>/dev/null; cargo test --bin cica substitut; cargo test --bin cica token`
Expected: PASS — `substitutes_memories_token_when_present`, `leaves_prompt_unchanged_when_token_absent`, `none_prompt_stays_none` all pass. (If the first form errors with "no library targets", ignore — use `cargo test --bin cica`.)

- [ ] **Step 7: Commit**

```bash
git add src/memory.rs src/sandbox/local.rs
git commit -m "feat(memory): substitute {MEMORIES_DIR} token to the worker-local path

The router emits a {MEMORIES_DIR} placeholder; LocalProcessProvider
resolves it to the local per-user memories dir in the process that runs
the agent, so writes land where HydratingProvider captures them.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Pull `mem/` from the store before reindexing (Change #2)

**Files:**
- Modify: `src/channels/mod.rs` (imports, `pull_memories_with_store` helper, `reindex_user_memories` made async, call site at line ~389)
- Test: `src/channels/mod.rs` (inline tests — add a `#[cfg(test)] mod` if none exists for this, else extend)

- [ ] **Step 1: Write the failing test for the pure pull helper**

In `src/channels/mod.rs`, add at the end of the file (before EOF) a test module — or extend an existing `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod memory_pull_tests {
    use super::*;
    use crate::sandbox::state::{FilesystemStateStore, StateStore};
    use std::sync::Arc;

    #[tokio::test]
    async fn pulls_from_store_into_dest() {
        let store_root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));
        // Seed a memory blob at mem/telegram_1.
        let seed = tempfile::tempdir().unwrap();
        std::fs::write(seed.path().join("note.md"), "remember this").unwrap();
        store.push(seed.path(), "mem/telegram_1").await.unwrap();

        let store_dyn: Option<Arc<dyn StateStore>> = Some(store);
        let pulled = pull_memories_with_store(store_dyn.as_ref(), dest.path(), "telegram", "1")
            .await
            .unwrap();
        assert!(pulled);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("note.md")).unwrap(),
            "remember this"
        );
    }

    #[tokio::test]
    async fn no_store_is_a_noop() {
        let dest = tempfile::tempdir().unwrap();
        let pulled = pull_memories_with_store(None, dest.path(), "telegram", "1")
            .await
            .unwrap();
        assert!(!pulled);
        // dest stays empty — single-box must not attempt any pull.
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin cica pulls_from_store_into_dest`
Expected: FAIL — `cannot find function pull_memories_with_store in this scope`.

- [ ] **Step 3: Add the pure pull helper**

In `src/channels/mod.rs`, add near `reindex_user_memories` (line ~1031):

```rust
/// Pull a user's memories from the state store into `dest`. `None` store
/// (single-box) is a no-op returning `Ok(false)` — never attempts a pull.
async fn pull_memories_with_store(
    store: Option<&std::sync::Arc<dyn crate::sandbox::state::StateStore>>,
    dest: &std::path::Path,
    channel: &str,
    user_id: &str,
) -> anyhow::Result<bool> {
    match store {
        Some(s) => s.pull(&format!("mem/{channel}_{user_id}"), dest).await,
        None => Ok(false),
    }
}
```

- [ ] **Step 4: Run the helper tests to verify they pass**

Run: `cargo test --bin cica memory_pull_tests`
Expected: PASS — `pulls_from_store_into_dest` and `no_store_is_a_noop` both pass.

- [ ] **Step 5: Make `reindex_user_memories` async and pull before reindexing**

In `src/channels/mod.rs`, replace the current `reindex_user_memories` (lines 1031-1045):

```rust
pub fn reindex_user_memories(channel: &str, user_id: &str) {
    match MemoryIndex::open() {
        Ok(mut index) => {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to re-index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
        }
    }
}
```

with:

```rust
pub async fn reindex_user_memories(channel: &str, user_id: &str) {
    // Cloud mode: S3 is authoritative for memories — pull this user's prefix so
    // the router index reflects what workers wrote. (Hand-edits on the router's
    // disk get clobbered by this pull; route operator edits through a turn or
    // straight to the store.) Single-box: no store → skipped. Best-effort.
    match crate::config::Config::load()
        .and_then(|cfg| crate::sandbox::state::default_store(&cfg))
        .and_then(|store| crate::memory::memories_dir(channel, user_id).map(|dir| (store, dir)))
    {
        Ok((store, dest)) => {
            if let Err(e) = pull_memories_with_store(store.as_ref(), &dest, channel, user_id).await {
                warn!(
                    "Failed to pull memories for {}:{} (reindexing local copy): {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => warn!("Failed to resolve store for memory pull {}:{}: {}", channel, user_id, e),
    }

    match MemoryIndex::open() {
        Ok(mut index) => {
            if let Err(e) = index.index_user_memories(channel, user_id) {
                warn!(
                    "Failed to re-index memories for {}:{}: {}",
                    channel, user_id, e
                );
            }
        }
        Err(e) => {
            warn!("Failed to open memory index: {}", e);
        }
    }
}
```

Note: `default_store` returns `Result<Option<Arc<dyn StateStore>>>`, so `store` here is `Option<Arc<dyn StateStore>>` and `store.as_ref()` yields `Option<&Arc<...>>` matching the helper signature.

- [ ] **Step 6: Update the call site to await**

In `src/channels/mod.rs` line ~389, change:

```rust
    reindex_user_memories(channel.name(), user_id);
```

to:

```rust
    reindex_user_memories(channel.name(), user_id).await;
```

- [ ] **Step 7: Build and run all memory tests**

Run: `cargo build --bin cica 2>&1 | tail -20`
Expected: compiles clean (no "await used in a non-async fn", no unused-import warnings).

Run: `cargo test --bin cica memory_pull_tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/channels/mod.rs
git commit -m "feat(memory): pull mem/ from the store before reindexing

In cloud mode the router pulls a user's memories from S3 (written by
workers) before reindexing, so they're searchable next turn. No-op in
single-box. Runs after the reply is sent — off the user-facing path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Rewrite the `## Memories` guidance with the routing rule (Change #3)

**Files:**
- Modify: `src/onboarding.rs` (lines ~569-589, the `## Memories` block; imports at line 21)
- Test: `src/onboarding.rs` (assert the rendered prompt contains the token + routing rule)

- [ ] **Step 1: Write the failing test**

First check how `build_context_prompt_for_user` is invoked. Read `src/onboarding.rs` around line 540-624 to confirm the signature and whether a simple call is testable without network. Then add to `src/onboarding.rs`'s test module (create `#[cfg(test)] mod tests` at end of file if none exists):

```rust
#[cfg(test)]
mod memory_guidance_tests {
    use super::*;

    #[test]
    fn guidance_emits_token_and_routing_rule() {
        let prompt = build_context_prompt_for_user(
            Some("Telegram".to_string()),
            Some("telegram"),
            Some("1"),
            None,
        )
        .expect("prompt builds");
        // Emits the placeholder, not a router-absolute path.
        assert!(prompt.contains(crate::memory::MEMORIES_DIR_TOKEN));
        // Routes durable org facts to propose-knowledge, not personal memory.
        assert!(prompt.contains("propose-knowledge"));
    }
}
```

If `build_context_prompt_for_user` requires more args or does IO that fails in tests, adapt: call it with the real signature and only assert on the `## Memories` substring it always renders for a known user. (Read the function first — do not guess the signature.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin cica guidance_emits_token_and_routing_rule`
Expected: FAIL — assertion fails (current text uses `mem_dir.display()`, no `{MEMORIES_DIR}`, no `propose-knowledge`).

- [ ] **Step 3: Rewrite the guidance block**

In `src/onboarding.rs`, replace the `## Memories` block (currently lines ~571-589, starting `lines.push("## Memories"...)` through the `DO ask before saving...` push). Remove the `mem_dir` binding at line 569 (`let mem_dir = memories_dir(ch, uid)?;`) since the path is no longer interpolated. Insert:

```rust
        // Memory guidance — personal vs. org-wide routing.
        lines.push("## Memories".to_string());
        lines.push(format!(
            "You have a per-user memory store at: {}",
            crate::memory::MEMORIES_DIR_TOKEN
        ));
        lines.push(String::new());
        lines.push("**Personal / user-specific** facts — this user's preferences, the projects they're driving, how they like answers, things they tell you about themselves — go in memory:".to_string());
        lines.push("1. Ask the user if they'd like you to remember it.".to_string());
        lines.push(format!(
            "2. If they agree, write a markdown file under {} with a descriptive name (e.g. `preferences.md`, `project-foo.md`), formatted with headers and bullets.",
            crate::memory::MEMORIES_DIR_TOKEN
        ));
        lines.push("Ask first; don't save trivia.".to_string());
        lines.push(String::new());
        lines.push("**Durable org-wide** facts — where a feature lives, a data/schema gotcha, a domain term, a repo-routing rule — do NOT go in personal memory. Offer to capture them in the shared knowledge corpus via the `propose-knowledge` skill (a Draft PR others review) instead.".to_string());
        lines.push(String::new());
```

Then handle the now-unused import: if `memories_dir` is no longer referenced anywhere in `onboarding.rs`, change line 21 from `use crate::memory::{MemoryIndex, memories_dir};` to `use crate::memory::MemoryIndex;`. (Run `grep -n memories_dir src/onboarding.rs` to confirm before removing.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --bin cica guidance_emits_token_and_routing_rule`
Expected: PASS.

- [ ] **Step 5: Build clean (no unused-import warning)**

Run: `cargo build --bin cica 2>&1 | tail -20`
Expected: compiles with no warnings about `memories_dir` being unused.

- [ ] **Step 6: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(memory): route personal facts to memory, org facts to propose-knowledge

Rewrites the memory guidance for the work-assistant era: personal facts
go to per-user memory (ask first); durable org facts go to the shared
corpus via propose-knowledge. Emits the {MEMORIES_DIR} token instead of a
router-absolute path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Full verification + deslop

**Files:** none (verification only)

- [ ] **Step 1: Full build + test**

Run: `cargo build --bin cica 2>&1 | tail -5 && cargo test --bin cica 2>&1 | tail -25`
Expected: build clean; all tests pass (including the three new test groups and the existing `hydrating` `memories_round_trip`).

- [ ] **Step 2: Clippy**

Run: `cargo clippy --bin cica 2>&1 | tail -25`
Expected: no new warnings from the touched files.

- [ ] **Step 3: Deslop pass**

Invoke the `deslop` skill on the diff (`git diff main...HEAD`). Apply any cleanups, re-run `cargo test --bin cica`, commit if changes were made.

- [ ] **Step 4: Confirm single-box parity by inspection**

Verify by reading the diff that: (a) `job_to_query_options` produces the identical string single-box (token resolves to the same `config::paths().base` path the prompt used to embed), and (b) `reindex_user_memories` skips the pull when `default_store` returns `None`. Both are covered by `none_prompt_stays_none`/`leaves_prompt_unchanged_when_token_absent` and `no_store_is_a_noop` respectively — note this in the PR description.

---

## Self-Review Notes (author)

- **Spec coverage:** §3 → Task 1; §4 → Task 2; §5 → Task 3; §6 error handling → warn-and-continue in Tasks 2/3; §7 tests → the three test groups + Task 4 live sign-off (manual, post-deploy); single-box parity invariant → Task 4 Step 4 + the no-store/no-token tests.
- **Deferred to deploy (not in this plan):** version bump, `v*` tag, `update-router.sh`, fleet deploy, and the live sign-off dogfood — these happen after merge, same as prior phases.
- **Type consistency:** `substitute_token(Option<&str>, &Path) -> Option<String>`; `pull_memories_with_store(Option<&Arc<dyn StateStore>>, &Path, &str, &str) -> Result<bool>`; `reindex_user_memories` becomes `async`. `MEMORIES_DIR_TOKEN` defined once in `memory.rs`, used in `local.rs` + `onboarding.rs`.
