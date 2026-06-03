# Phase 2: Externalize Session + Memory State (`StateStore`)

**Date:** 2026-06-02
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessor:** Phase 1 (`SandboxProvider` seam) — shipped in 0.5.0.

## Goal

Build and **prove** a capture/restore round-trip for the durable state a turn touches — the Claude/Cursor session and the user's `memories/` — so a later phase can run turns on ephemeral remote workers without losing context. Phase 2 runs entirely on one box with the existing `LocalProcessProvider`; the externalized store adds no functional benefit yet, only the verified mechanism Phase 3 depends on.

## Operating model (frame for all phases)

- **The binary owns the mechanisms; the operator owns the topology.** cica ships every backend behind traits, selected by config. Zero distributed config → behaves exactly like today (one self-contained box, all-local).
- **Cloud backends are `--features`-gated.** The default build (what `curl | sh` and `cargo build --release` produce) is lean and local-only, with no cloud SDKs. Distributed capability is opt-in at build time.
- **Worker is the same binary** (Phase 3: `cica worker`). cica ships the worker image + a deployment contract; operators provision topology with their own IaC (CDK/Terraform/Pulumi). cica's launcher *invokes* pre-registered task definitions; it never *creates* infrastructure.

## Scope

**In scope (Phase 2):**
- `StateStore` trait + `FilesystemStateStore` (always compiled, no cloud SDKs).
- `SessionArtifacts` resolver (claude backend) that maps a logical `session_id` to the set of on-disk files, and captures/restores them.
- Per-user `memories/` directory sync.
- A `HydratingProvider` decorator that wraps a `SandboxProvider`: pull state → run inner turn → push state.
- Wiring in `default_provider` so a configured store enables hydration; no configured store = today's bare `LocalProcessProvider`.
- A `capture → wipe → restore → --resume` round-trip integration test.

**Out of scope (Phase 3+):**
- `cica worker` subcommand; container/Fargate/Cloud Run launchers.
- Real S3/GCS `StateStore` impls (feature-gated) and their cloud SDKs.
- Worker `Dockerfile` + deployment contract doc.
- Returning a worker's result to the router; cwd canonicalization.
- Cursor `SessionArtifacts` (leave a seam; implement claude only now).

## Background: what a "session" is on disk

A Claude session is **not a single file**. Under `$CLAUDE_HOME/.claude/`, for a given `session_id`:
- `projects/<cwd-slug>/<session_id>.jsonl` — the conversation transcript (required for `--resume`)
- `session-env/<session_id>` — session environment
- `todos/<session_id>-agent-*.json` — in-flight todos
- (`.claude.json` holds a global project/session registry; `shell-snapshots/`, etc.)

`<cwd-slug>` is the slugified working directory (e.g. `-Users-dcvz-Library-Application-Support-cica`). `claude --resume <id>` locates the transcript via the slug of the *current* cwd. `$CLAUDE_HOME` is `config::paths().claude_home` (`internal/claude-home`). Per-user memories live at `paths.base/users/{channel}_{user_id}/memories/` (flat markdown).

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Backends now | `FilesystemStateStore` only | Proves the round-trip; zero cloud-SDK weight; default build unchanged. S3/GCS are feature-gated Phase 3. |
| Store key | Logical `session_id` (and `channel:user_id` for memories) — **not** the cwd-slug | Decouples storage from working directory. Restore writes into the slug dir computed from the *current* cwd, so a Phase 3 worker with a different cwd recomputes its own slug and `--resume` still finds the transcript. No forced canonical cwd in Phase 2. |
| Capture set | transcript `.jsonl` (required) + `session-env/<id>` + `todos/<id>-*.json`; expand if the round-trip test shows resume needs more (e.g. a `.claude.json` entry) | The minimal correct set is an empirical question; a test discovers it rather than a guess. |
| Hydration wiring | `HydratingProvider` decorator around the inner provider | Provider-agnostic; reused/relocated into `cica worker` in Phase 3. |
| Default behavior | No store configured → bare `LocalProcessProvider`, no hydration | Preserves today's single-binary behavior exactly. |
| Backend coverage | claude only; `SessionArtifacts` trait leaves a cursor seam | YAGNI; claude is the primary path. |

## Components

### `StateStore` trait + `FilesystemStateStore`

```rust
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Pull everything under `key` into `dest`. Returns false if the key is absent.
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool>;
    /// Push the contents of `src` to `key` (replacing what's there).
    async fn push(&self, src: &Path, key: &str) -> Result<()>;
}
```
- `FilesystemStateStore { root: PathBuf }` (e.g. `internal/state-store/`) copies directory trees between `root/<key>/…` and the working location. Keys are path-safe strings.
- Phase 3 adds `#[cfg(feature = "s3")] S3StateStore` and `#[cfg(feature = "gcs")] GcsStateStore` behind the same trait. `default_store(&Config)` selects by config; if config requests a backend the binary wasn't built with, fail fast at startup with a clear message.

### `SessionArtifacts` resolver

```rust
pub trait SessionArtifacts {
    /// Files (relative to claude_home) that make up `session_id` for the given cwd.
    fn artifact_paths(&self, claude_home: &Path, cwd: &Path, session_id: &str) -> Vec<PathBuf>;
}
```
- `ClaudeSessionArtifacts` resolves the transcript (`.claude/projects/<slug(cwd)>/<id>.jsonl`), `session-env/<id>`, and `todos/<id>-*.json`. The slug function mirrors Claude Code's cwd→slug rule.
- Capture copies these into a staging dir laid out **relative to a normalized root** (so the slug is reconstructed on restore from the current cwd, not stored literally). Restore reverses it into the live `claude_home`.

### `HydratingProvider`

```rust
pub struct HydratingProvider<P: SandboxProvider> {
    inner: P,
    store: Arc<dyn StateStore>,
}
```
`run_turn(job)`:
1. **Hydrate:** `store.pull(session_key(job), …)` into `claude_home` (skip if absent → fresh session); `store.pull(memories_key(job), …)` into the user's `memories/`.
2. **Run:** `inner.run_turn(job)` (the `LocalProcessProvider` subprocess).
3. **Dehydrate:** capture the session artifacts for the resulting `backend_session_id` and `store.push` them; `store.push` the `memories/` dir.
4. Return the inner `TurnResult`.

`session_key` = the logical session id; `memories_key` = `mem/{channel}_{user_id}`.

### Wiring

`default_provider(&Config)` (extended from Phase 1):
- store configured → `HydratingProvider::new(LocalProcessProvider, default_store(config))`
- no store → `LocalProcessProvider` (unchanged).

A new optional config field selects the store (e.g. `[deployment] store = "filesystem"` with a path; absent = none).

## Data flow (per turn, Phase 2)

```
router turn → default_provider → HydratingProvider
  pull session_id  → claude_home/.claude/projects/<slug>/<id>.jsonl (+env,+todos)
  pull mem/<user>  → users/<user>/memories/
  LocalProcessProvider.run_turn → claude --resume <id> (subprocess, same box)
  capture artifacts(new backend_session_id) → push session_id
  push users/<user>/memories/ → mem/<user>
→ router re-indexes memories (fastembed), posts response
```

## Error handling

- **Absent key on pull:** not an error — treated as "no prior state" (fresh session / empty memories).
- **Pull failure (present but unreadable):** turn-level error surfaced to the channel; do not run the subprocess against partial state.
- **Push failure on dehydrate:** turn-level error; prior durable state in the store is left intact (no partial-transcript commit — push transcript only after a successful inner turn, and treat a failed push as a failed turn).
- **Crash mid-turn:** only the live `claude_home`/scratch is affected; the store's last good state is unchanged.

## Testing strategy

- **Round-trip resume (headline):** create a session via the local provider; capture artifacts to a `FilesystemStateStore`; wipe `claude_home`; restore; run a follow-up turn with `--resume` and assert it continues the conversation. This empirically validates the capture set; expand the set if it fails.
- **`StateStore` contract test:** `pull` of absent key → false; `push` then `pull` round-trips a directory tree byte-for-byte; overwrite semantics.
- **`SessionArtifacts` resolver:** given a known `claude_home` layout + cwd + id, returns exactly the expected paths; slug function matches Claude Code's rule for representative cwds.
- **`HydratingProvider`:** with a fake in-memory/temp `StateStore` and a stub inner provider, asserts pull-before / push-after ordering and that an absent session hydrates to fresh.
- **Memory parity:** after dehydrate + re-index, `MemoryIndex::search` returns memories the agent wrote during the turn.

## Distribution impact

**Phase 2: none.** `FilesystemStateStore` and the decorator are always compiled with no new dependencies; the store is off unless configured. `cargo build --release` (no features), the `release.yml` artifacts, `install.sh`, and `cargo install --git` are all unchanged.

**Distribution contract (binding for Phase 3):**
- The **default binary never gains cloud dependencies.** The `curl | sh` artifact and `cargo build --release` remain lean and local-only; cloud backends are strictly `--features s3` / `--features gcs` (umbrella `--features cloud`).
- Phase 3 will add: feature-gated cloud builds as **additional, separately-named release artifacts** (Linux-only is sufficient — workers don't run on macOS); a `--cloud` / `CICA_VARIANT` switch in `install.sh`; a **worker container-image publish workflow** (the primary cloud artifact); and `--all-features` build + clippy coverage in CI so gated code is compiled and linted.
- A config request for a backend the binary wasn't built with must **fail fast at startup** with an actionable message (which feature to enable).

**Pre-existing follow-up (not Phase 2 scope):** `install.sh` and the README reference `oxideai/cica` (download base URL, version-check API, banner) while the repo and 0.5.0 release live at `oxiglade/cica`. If releases are published under `oxiglade`, the installer downloads from the wrong org. Track and fix separately.

## Open questions / future work

- Exact minimal capture set (resolved by the round-trip test during implementation).
- Whether memories sync should diff/delta rather than replace the prefix (premature now; revisit if memory dirs grow large).
- Cursor `SessionArtifacts` implementation (when cursor parity is needed).
