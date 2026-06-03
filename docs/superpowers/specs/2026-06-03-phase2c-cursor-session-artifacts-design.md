# Phase 2c: Cursor Session Artifacts

**Date:** 2026-06-03
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 2 (`StateStore` + `HydratingProvider` + `ClaudeSessionArtifacts`), Phase 3a (`cica worker` + dispatch).

## Why this phase exists

The Phase 3a validation gate caught a real gap: the production deployment runs the **Cursor backend**, but session capture/restore was implemented **Claude-only**. `HydratingProvider` skips session hydration/capture for any non-Claude backend (`is_claude` gate), so Cursor sessions were never persisted to the `StateStore`. Cursor conversations only "remembered" via the **shared local `cursor-home`** on one box — which would break in a fresh worker container.

This phase implements `CursorSessionArtifacts` and a backend-dispatched `SessionArtifacts` trait so the worker fleet persists/restores Cursor sessions. It unblocks the 3a validation gate for the backend actually in use, before any cloud (3b) work.

## Verified facts (investigated on the live deployment)

- A Cursor session is stored **locally** as a SQLite database:
  `$CURSOR_HOME/.cursor/chats/<workspace_hash>/<session_id>/store.db` (+ `store.db-wal`, `store.db-shm`).
- `<session_id>` is the same id cica tracks in `pairing.json` and passes to `cursor --resume <id>`.
- `<workspace_hash>` is **`md5(realpath(cwd))`** — verified: `md5("/data/cica") = 5c64d42749f92f28359bff54fe4cb4bc`, the live workspace dir. It is a **pure function of the resolved working directory** — no machine/account component. (This is Cursor's analog of Claude's path-slug.)
- Cursor runs with `HOME = paths.cursor_home` and `cwd = options.cwd.unwrap_or(paths.base)` (`src/backends/cursor.rs`). Auth is an **API key** passed as `--api-key` from config (not the keychain) — so a containerized Cursor worker needs only `config.cursor.api_key`.
- `paths.base` on the prod box resolves to `/data/cica` (a symlink from `~/.config/cica`), which is why slugs/hashes say `data-cica`.

## The fleet portability guarantee (the load-bearing property)

Because the Cursor workspace hash is `md5(realpath(cwd))` and Claude's slug is a pure function of cwd, **two worker machines that run the agent with the same resolved cwd produce the same on-disk session location.** Therefore a session captured by one worker restores exactly where another worker's `--resume` looks. This makes cross-worker session portability a function of **cwd consistency**, which we control.

**3b deployment-contract requirement (recorded here):** every worker must run the agent subprocess with the **same resolved cwd**. The container image pins `paths.base` to a constant real path (deterministic `HOME`/`XDG`, no per-container symlink divergence). The chosen canonical cwd should be **`/data/cica`** to match the current prod box so in-flight sessions migrate seamlessly; choosing a different path is allowed but resets in-flight sessions at cutover (the store is the source of truth going forward).

## Design

### 1. `SessionArtifacts` trait + backend dispatch
Introduce a trait abstracting the backend-specific capture/restore:

```rust
pub trait SessionArtifacts {
    /// Copy the files making up `session_id` from `home` into `staging`.
    /// Returns false (capturing nothing) if the session isn't found.
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool>;
    /// Restore staged artifacts into `home` so a resume run with `cwd` finds them.
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()>;
}
```
- `ClaudeSessionArtifacts` — the existing capture/restore logic, moved behind the trait (behavior unchanged). `home` = claude_home.
- `CursorSessionArtifacts` — new (below). `home` = cursor_home.

`HydratingProvider` selects the impl **and the home** by `job.backend`:
- `Claude` → `ClaudeSessionArtifacts`, home = `claude_home`
- `Cursor` → `CursorSessionArtifacts`, home = `cursor_home`

The `is_claude` skip is removed; both backends now hydrate/dehydrate sessions. (Memory sync is already backend-agnostic and unchanged.) `HydratingProvider` gains a `cursor_home` field alongside `claude_home`.

### 2. `CursorSessionArtifacts`
- **Capture:** glob `home/.cursor/chats/*/<session_id>/` (hash-independent). Copy the session dir's SQLite files — `store.db`, `store.db-wal`, `store.db-shm` (whichever exist) — into `staging/<workspace_hash>/`, **recording the workspace-hash dir name**. Return false if no matching session dir.
  - Copying all three files together preserves an un-checkpointed WAL; the cursor process has already exited (quiescent), and SQLite replays the WAL on next open.
- **Restore:** read the recorded workspace-hash; copy the SQLite files back to `home/.cursor/chats/<recorded_hash>/<session_id>/`.

### 3. Workspace-hash: record-and-replay (mechanism) + md5 proof (guarantee)
The mechanism is **record-and-replay**: capture stores the hash dir name; restore replays it verbatim. We do **not** reimplement `md5(realpath(cwd))` in code — record-replay needs no md5 dependency and is **resilient if Cursor changes its hashing**. Its correctness rests on the verified fact that the hash is a pure function of cwd plus the consistent-cwd requirement above: every worker computes the same hash, so the replayed dir is exactly where `--resume` looks.

## Components / file structure

- Modify `src/sandbox/artifacts.rs` — add the `SessionArtifacts` trait; make `ClaudeSessionArtifacts` implement it (wrap existing fns); add `CursorSessionArtifacts`.
- Modify `src/sandbox/hydrating.rs` — add `cursor_home` field; dispatch artifacts + home by `job.backend`; remove the session-skip for non-Claude (keep a warn only for genuinely-unsupported backends, if any).
- Modify `src/sandbox/mod.rs` (`try_default_provider`) and `src/cmd/worker.rs` — pass `paths.cursor_home` into `HydratingProvider::new` (now `(inner, store, claude_home, cursor_home, cwd)`).

## Data flow (Cursor turn, in a worker)

```
worker: pull job
  HydratingProvider (backend = Cursor):
    hydrate: pull session/<id> ← store → CursorSessionArtifacts.restore → cursor_home/.cursor/chats/<hash>/<id>/store.db*
             pull mem/<user> ← store
    run: cursor --resume <id> (HOME=cursor_home, cwd=paths.base)
    dehydrate: CursorSessionArtifacts.capture(cursor_home, new_id) → push session/<new_id> → store
               push mem/<user> → store
  push result
```

## Error handling

- **Capture finds no session dir** → returns false → nothing pushed (same as Claude). Not an error.
- **Missing `-wal`/`-shm`** (SQLite checkpointed) → copy whatever exists; `store.db` alone is valid.
- **Restore with absent stored session** → hydrate pull returns false → restore skipped → `--resume` falls back to whatever's local (fresh in a clean worker). Same shape as Claude.

## Testing strategy

- **`CursorSessionArtifacts` round-trip (unit):** build a synthetic `cursor-home` with `.cursor/chats/<hash>/<id>/store.db(+wal+shm)`; capture → wipe → restore into a fresh home; assert the files reappear under the **recorded** `<hash>/<id>/` with identical bytes. Capture-returns-false when the session is absent.
- **`SessionArtifacts` dispatch (unit):** `HydratingProvider` with a stub inner provider + fake store routes a `Cursor` job to `CursorSessionArtifacts` (and `Claude` to `ClaudeSessionArtifacts`), using the correct home. Reuse the Phase-2 hydrating test harness (tempdirs + stub provider).
- **Claude parity:** existing Claude artifact tests still pass after moving behind the trait.
- **Manual validation (clears the 3a gate for Cursor):** with `provider=subprocess` + `store=filesystem`, send a message; confirm `session/<id>/` now appears in the store. Then **wipe `cursor-home/.cursor/chats/`**, send a follow-up in the same conversation, and confirm it resumes from the store-restored db (the real test the same-box false positive masked). Confirm memory persists.

## Distribution impact

None. No new dependencies (record-and-replay avoids an md5 crate). Additive, behind the existing opt-in `[deployment]` config.

## Out of scope (later)

- 3b: containerizing the worker, the cwd-pinning in the image, passing `cursor.api_key`/secrets to the worker, Fargate/S3.
- Reverse-engineering or recomputing Cursor's hash in code (unnecessary given record-and-replay).
- Cloud-vs-local sufficiency of `store.db`: if the manual wipe-and-resume test shows Cursor *also* needs cloud state, that's fine (the worker has cloud access); capturing the local db remains correct.
