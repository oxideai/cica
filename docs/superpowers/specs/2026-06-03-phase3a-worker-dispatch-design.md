# Phase 3a: Worker Process + Store-Mediated Dispatch (no cloud)

**Date:** 2026-06-03
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 1 (`SandboxProvider` seam, 0.5.0), Phase 2 (`StateStore` + `HydratingProvider`, PR #7 — this branch is stacked on it).

## Goal

Build the worker process (`cica worker`) and the store-mediated dispatch protocol that the cloud launchers will reuse, proving the entire worker round-trip on a single box with no Docker, no cloud, and no feature flags. Like Phase 2's `FilesystemStateStore`, the local `SubprocessWorkerProvider` exists to validate the mechanism so Phase 3b's container launchers can rely on it.

## Scope decomposition (Phase 3 overall)

Phase 3 from the parent design was too large for one spec; it is split into:
- **3a (this spec):** `cica worker` subcommand + store-mediated dispatch + `SubprocessWorkerProvider` (local child process). No cloud.
- **3b:** worker `Dockerfile`; `ContainerProvider` + `Launcher` trait + AWS Fargate; feature-gated `S3StateStore`; network result-return; deployment-contract doc; feature-gated cloud release artifacts + `--all-features` CI.
- **3c:** GCP Cloud Run launcher + `GcsStateStore`.
- **Separate track (not Phase 3):** git-backed skills + `publish_skill` — orthogonal to worker/launcher infra.

## Operating model (unchanged from parent)

One binary, two roles selected by config. Zero distributed config → today's in-process behavior. The launcher (how a worker is started) is the only thing that varies across 3a/3b/3c; the job/result protocol is identical.

## Roles

- **Router** = `cica` (today's long-lived process). With `[deployment] provider = "subprocess"`, it dispatches each turn to a worker instead of running it in-process.
- **Worker** = `cica worker --turn <turn_id>` — runs exactly one turn, then exits.
- **Default** (`provider` unset or `"local"`) = today's in-process behavior (Phase 1/2). Strictly opt-in; no behavior change for existing deployments.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Job/result transport | **Store-mediated, keyed by `turn_id`** | The one channel a fire-and-forget cloud task can also use; unbounded (large system prompts won't fit in env/args). 3a exercises the exact protocol 3b reuses. |
| Dispatch trait fit | `SubprocessWorkerProvider` is a `SandboxProvider` | Drops into `query_ai_with_session` unchanged; router keeps owning the session registry. |
| Where hydration runs | **Inside the worker** (`HydratingProvider` from Phase 2) | The worker owns its state; the router doesn't hydrate when `provider=subprocess`. Matches the parent design's "relocate the seam into the worker." |
| Job/result storage | Reuse `StateStore` dir-tree `pull`/`push`; job/result are one-file dirs | No `StateStore` trait change. |
| `provider=subprocess` without a store | Fail fast at startup | The protocol depends on the store. |
| Worker cwd | **Fresh per-turn scratch dir** (ephemeral workspace) | Phase 2 slug-decoupling handles a varying cwd (capture-by-id, restore-under-`slug(cwd)`). Validated by the worker round-trip; canonicalize only if resume proves cwd-sensitive. |
| Worker failure | **Surface a clean error to the channel; no fallback to in-process** | The operator opted into workers; masking failures hides real problems and defeats isolation. |

## Components

### `cica worker` subcommand
- `src/cmd/worker.rs`, registered in `src/main.rs` and `src/cmd/mod.rs`.
- CLI: `cica worker --turn <turn_id>`.
- Flow:
  1. `Config::load()`; build the `StateStore` via `sandbox::state::default_store(&config)?` (error if `None` — a worker requires a store).
  2. `pull` `turns/<turn_id>/job` into a temp dir; deserialize `TurnJob` from `job.json`.
  3. Build the engine: `HydratingProvider::new(LocalProcessProvider::new(), store, claude_home, scratch_cwd)` where `scratch_cwd` is a fresh per-turn dir. Set the job's effective cwd to `scratch_cwd`.
  4. `run_turn(job)` → `TurnResult`.
  5. Serialize `TurnResult` to a temp dir as `result.json`; `push` to `turns/<turn_id>/result`.
  6. Exit 0. On any error: log and exit non-zero (do NOT write a result).

### `SubprocessWorkerProvider` (router side)
- `src/sandbox/worker.rs`. Implements `SandboxProvider`.
- Fields: `store: Arc<dyn StateStore>`, `self_exe: PathBuf` (the cica binary path, from `std::env::current_exe()`).
- `run_turn(job)`:
  1. `turn_id = Uuid::new_v4()`.
  2. Serialize `job` → temp dir `job.json`; `store.push(tmp, "turns/<turn_id>/job")`.
  3. Spawn `self_exe worker --turn <turn_id>` (inheriting env so the child `Config::load()` sees the same config); await exit.
  4. If exit ≠ 0 → `bail!` (turn error).
  5. `store.pull("turns/<turn_id>/result", tmp)`; if absent → `bail!`. Deserialize `result.json` → `TurnResult`.
  6. Best-effort delete `turns/<turn_id>/` from the store (cleanup); return the result.

### Serialization
`TurnJob` and `TurnResult` (`src/sandbox/mod.rs`) gain `#[derive(Serialize, Deserialize)]`. `AiBackend` already derives `Serialize, Deserialize`. Job/result are written as a single `*.json` file inside the key's directory so the dir-based `StateStore` carries them unchanged.

### Config + wiring
- `src/config.rs`: add `provider: Option<ProviderKind>` to `DeploymentConfig`; `enum ProviderKind { Local, Subprocess }` (serde lowercase). `None` ⇒ `Local`.
- `src/sandbox/mod.rs` `default_provider`:
  - `provider = Local` (or unset): today's behavior — `HydratingProvider(local, store)` if a store is configured, else bare `LocalProcessProvider`.
  - `provider = Subprocess`: require a store (else the factory returns an error / the router logs and refuses to start); return `SubprocessWorkerProvider`.
- Startup validation: `provider=subprocess` with no `store` → clear, actionable error.

## Data flow (per turn, `provider=subprocess`)

```
channel → query_ai_with_session → SubprocessWorkerProvider.run_turn(job)
  push turns/<id>/job  ──▶ StateStore (filesystem)
  spawn `cica worker --turn <id>`  (child process)
        worker: pull job ← store
                HydratingProvider: pull session+memories ← store
                  LocalProcessProvider → claude --resume (scratch cwd)
                  capture + push session, push memories ──▶ store
                push turns/<id>/result ──▶ store ; exit 0
  await child exit (0)
  pull turns/<id>/result ← store → TurnResult
  delete turns/<id>/
→ router records backend_session_id, posts response
```

## Error handling

- **Worker exit ≠ 0 / missing result blob / deserialize failure:** `SubprocessWorkerProvider.run_turn` returns `Err`; `query_ai_with_session` surfaces the existing "Sorry, I encountered an error" path. No in-process fallback.
- **Job push failure (before spawn):** turn error; nothing launched.
- **Store unconfigured with `provider=subprocess`:** startup error, router refuses to run.
- **Worker internal turn error:** the worker exits non-zero without writing a result; the router treats it as a failed turn (above). Durable state is only written by the worker's own dehydrate on success.
- **Cleanup failure (deleting `turns/<id>/`):** logged, non-fatal (orphaned turn blobs are harmless; a future GC can sweep them).

## Testing strategy

- **Job/result serialization round-trip:** `TurnJob`/`TurnResult` serialize→deserialize equal; written/read as `job.json`/`result.json` through a `FilesystemStateStore`.
- **`SubprocessWorkerProvider` dispatch (integration):** point `self_exe` at a small test helper binary (or a test that stubs spawn) that reads the job from the store and writes a canned result; assert `run_turn` returns it and cleans up `turns/<id>/`. If stubbing spawn is impractical, cover the push-job / read-result / cleanup logic with the child step faked.
- **Config validation:** `default_provider` / startup errors when `provider=subprocess` and no store; returns `SubprocessWorkerProvider` when both set; returns Phase-2 behavior when `provider=local`.
- **Worker subcommand (integration, stubbed backend):** invoke the worker's run function against a filesystem store with a stub inner provider, asserting it reads the job and writes a result. The real `claude --resume` round-trip remains the documented manual test — now exercised end-to-end through a worker with a scratch cwd (this also validates the cwd decision).

## Manual validation (run in a configured environment)

With `[deployment] provider = "subprocess"` and `store = "filesystem"`:
1. Send a message; confirm a `cica worker` child process runs and the reply arrives.
2. Confirm `turns/<id>/` is created and then cleaned up.
3. Send a follow-up in the same conversation; confirm context resumes (worker hydrated the session under the scratch cwd's slug).
4. Confirm a memory written in step 1 persists.
If resume fails specifically because the cwd differs per turn, introduce a fixed canonical worker cwd and re-test.

## Distribution impact

None. No new dependencies; `cica worker` is a subcommand of the same binary. Default build and all install paths unchanged. (Feature-gated cloud artifacts arrive in 3b.)

## Open questions / future work (3b+)

- Network result-return + task-status polling (Fargate `DescribeTasks`) — 3b.
- Passing config/creds to a containerized worker via env/secrets — 3b.
- A GC sweep for orphaned `turns/<id>/` blobs if cleanup ever fails at scale — revisit if needed.
- Canonical worker cwd, only if the manual resume test shows cwd-sensitivity.
