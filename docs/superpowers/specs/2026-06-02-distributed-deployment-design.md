# Cica Distributed Deployment — Control Plane + Worker Fleet

**Date:** 2026-06-02
**Status:** Design approved, pending spec review
**Inspiration:** [Shopify Engineering — "Under the River"](https://shopify.engineering/under-the-river) (River/Aquifer: separate the always-on harness from ephemeral sandboxes; "the session is the thing that must survive"; sandboxes are cattle, not pets).

## Problem

Cica runs as a single always-on binary that does two jobs with opposite resource profiles:

1. **Front door (lightweight, must be ~100% up):** channel listeners for Telegram/Signal/Slack, the Slack HTTP event server, pairing, and the cron clock. Sips resources but cannot miss a message.
2. **Workload (heavy, bursty):** every message spawns a `claude`/`cursor` subprocess (`bun run claude-code …`) that runs shell, file I/O, and web tools. Memory also does local ONNX embeddings via `fastembed`.

Because both live on one box, the deployment must be sized for the **sum of concurrent agent bursts** even though that capacity is used a few percent of the time. The confirmed pain drivers are the **agent subprocess** and **concurrency across many users**. The result is paying 24/7 for peak burst capacity.

## Goals

- Keep the always-on footprint small and cheap; push heavy, bursty agent execution onto on-demand compute that **scales to zero** when idle and **scales out** under concurrent load.
- Hard isolation between users' executions (multi-user safety).
- **Portable workers** (plain container/microVM) and **deployable to both AWS and GCP** — cloud choice is configuration, not a code fork.
- Preserve cica's "self-contained single binary" path for dev/homelab use (no infra required to run locally).

## Non-Goals

- Durable per-user working-file workspace. Workspace is **ephemeral scratch per turn** (see Decisions). May be added later as opt-in without rework.
- Reimplementing the agent harness (no splitting the Claude Code/Cursor loop from tool execution). The CLI backends remain the harness.
- Warm/sticky session-affinity workers. Deferred; interfaces are shaped so it can be layered on later.

## Key Decisions (locked during brainstorming)

| Decision | Choice |
| --- | --- |
| Overall shape | Control plane (router) + ephemeral worker fleet (River/Aquifer shape) |
| Worker substrate | Portable container/microVM behind a `SandboxProvider` trait; AWS **and** GCP |
| Interaction model | **A — stateless rehydrating workers**: dispatch a turn job, hydrate state, run, dehydrate, exit. Interfaces shaped so warm-affinity (B) is a later optimization. |
| Durable state | Conversation/session history, per-user memory, skills |
| Workspace | Ephemeral scratch per turn (durability flows through session + memory + skills + artifacts sent to the user) |
| Memory | Files-plus-derived-index, **not** an in-turn service call. Read router-side at prompt-build; written by the agent as markdown into the user's `memories/` dir; re-indexed router-side after the turn. `fastembed` stays router-side. |
| Skills | **Git-backed, read-only at runtime, draft-and-publish.** Source of truth = git repo at a pinned ref; workers read-only; agent drafts in scratch (usable in-session immediately); a `publish` action commits to the repo so the skill becomes shared + versioned. |

## Architecture

```
                 ┌─────────────────────────────────────┐
   Slack/TG/     │  ROUTER  (tiny, always-on)           │
   Signal  ─────▶│  • channel listeners + Slack HTTP    │
                 │  • pairing, cron clock               │
                 │  • session registry                  │
                 │  • memory index + fastembed (read)   │
                 │  • SandboxProvider dispatch          │
                 │  • skills publish (git commit)       │
                 └───────────────┬─────────────────────┘
                                 │ run_turn(job)
                                 ▼
                 ┌─────────────────────────────────────┐
                 │  WORKER  (ephemeral container)        │   ← scales 0..N
                 │  • hydrate: transcript + memories/    │
                 │    ◀── object store                   │
                 │    skills ◀── git (pinned ref, RO)    │
                 │  • run claude/cursor subprocess       │
                 │    (shell/files in throwaway scratch) │
                 │  • dehydrate: transcript + memories/  │
                 │    ──▶ object store                   │
                 │  • return response + usage            │
                 └─────────────────────────────────────┘

   State stores:  Object store (S3 │ GCS) — transcripts, per-user memories/
                  Git repo (pinned ref)   — skills (read-only at runtime)
                  Session registry        — session_key → backend_session_id
```

The router is sized for steady-state listening; workers carry the agent bursts. No message → no workers → no burst cost.

## The Seam in Existing Code

The refactor pivots on **one existing choke point**: `backends::query_with_options`, orchestrated by `query_ai_with_session` (`src/channels/mod.rs:1008`) and also used by `execute_cron_job` (`src/channels/mod.rs:955`). Today it spawns a local subprocess. It is changed to **dispatch a turn to a `SandboxProvider`**.

Everything above the seam is unchanged:
- Channel handlers (`src/channels/*.rs`)
- Session bookkeeping: the `PairingStore.sessions` map keyed `channel:user_id` (`src/channels/mod.rs:1016-1078`)
- Cron (`src/cron/*`, `execute_cron_job`)
- Prompt building incl. router-side memory search (`build_context_prompt_for_user` → `MemoryIndex::search`, `src/onboarding.rs:622-655`)

This is why the change is incremental rather than a rewrite.

## Components

### `SandboxProvider` trait (portability core)

```rust
#[async_trait]
trait SandboxProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult>;
}

struct TurnJob {
    session_id: String,        // logical cica session (from session_key)
    channel: String,
    user_id: String,
    prompt: String,
    system_prompt: Option<String>,
    model: Option<String>,
    backend: AiBackend,        // Claude | Cursor
    skip_permissions: bool,
    state_handle: StateHandle, // scoped, short-lived access to this user's state
}

struct TurnResult {
    response: String,
    backend_session_id: String,
    cost_usd: Option<f64>,
    duration_ms: Option<u64>,
}
```

Implementations:

- **`LocalProcessProvider`** — reproduces today's behavior exactly: spawn the subprocess, use local state. Keeps the single-binary dev/homelab path with zero infra. **Default.**
- **`ContainerProvider`** — launches an ephemeral container per turn via a `Launcher` sub-trait:
  - `FargateLauncher` (AWS ECS/Fargate task)
  - `CloudRunJobLauncher` (GCP Cloud Run Job)
  - Same worker image on both clouds. Cloud = config.

### `StateStore` trait (object storage)

```rust
#[async_trait]
trait StateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<()>;
    async fn push(&self, src: &Path, key: &str) -> Result<()>;
}
```
- `S3Store` (AWS) / `GcsStore` (GCP).
- Holds: per-session transcript (Claude Code/Cursor session JSONL) and per-user `memories/`.
- Keyed by `channel:user_id` and logical session id.

### Session registry

The existing `PairingStore.sessions` map (`session_key → backend_session_id`). Starts as today's on-router file; swappable to Postgres if a single router is outgrown. The logical `session_id` maps to the transcript object key.

### Memory (router-side index over durable markdown)

- **Source of truth:** per-user `memories/*.md` files in the object store (durable).
- **Read:** router-side at prompt-build (`MemoryIndex::search`), unchanged in spirit.
- **Write during turn:** the agent writes markdown into the hydrated `memories/` dir (existing behavior driven by the system prompt).
- **After turn:** worker pushes `memories/` back; router re-indexes (`reindex_user_memories`, `src/channels/mod.rs:1100`), recomputing embeddings with `fastembed`.
- The vector index is a **disposable derived artifact**; the markdown is the source of truth. No ONNX model in the worker image.

### Skills (git-backed, read-only runtime, draft-and-publish)

- **Source of truth:** a git repo at a **pinned ref** (e.g. GitHub). Cloud-agnostic by construction.
- **Runtime:** workers fetch the pinned ref **read-only** (or use a bundle built from it). Workers never mutate shared skill state.
- **Authoring:** the agent drafts a new skill into its **throwaway scratch during the turn**, so it is immediately usable *within that conversation*.
- **Publish:** a distinct `publish_skill` action has the **router commit** the drafted skill to the skills repo (direct commit on the chosen ref). Subsequent turns on every cloud pick it up at the new ref. Git creds live on the router only.
- Result: conversational "build a skill with me right now" UX preserved; published skills become versioned, diffable, revertible, auditable. No mutable shared disk, no concurrent-write hazard, no sync-back.

## Turn Lifecycle (Approach A)

1. Message arrives. Router builds the context prompt (incl. router-side memory search) and records the turn against the session.
2. Router calls `provider.run_turn(job)`, passing a `state_handle` (short-lived signed URLs / scoped creds for this user's object-store prefix and the pinned skills ref).
3. Worker **hydrates**: pulls the transcript into the local `claude` home, pulls `memories/`, fetches skills read-only at the pinned ref.
4. Worker runs `claude --resume <backend_session_id>` (or a new session) — shell/file work happens in ephemeral scratch.
5. Worker **dehydrates**: pushes the updated transcript and any new/edited `memories/` back; returns `TurnResult`. Container exits.
6. Router persists `backend_session_id`, re-indexes memory, and posts the response to the channel. On `publish_skill`, the router commits to the skills repo.

## Concurrency, Isolation, Scaling

- One turn = one container = hard isolation between users. One user's shell cannot observe another's.
- Router caps maximum concurrent workers (config); excess turns queue.
- Idle → zero workers. Concurrent load → up to N workers. The router is sized for steady-state listening, not peak agent bursts.

## Cron

`execute_cron_job` dispatches through the same `SandboxProvider`. A cron turn is just a turn with no human in the channel; results post to the configured target.

## Error Handling

- **Worker launch failure / timeout:** router falls back to the existing "expired session → retry fresh" path (`src/channels/mod.rs:1045`), then surfaces a clean error to the channel.
- **Worker crash mid-turn:** only that turn's ephemeral scratch is lost; durable state (transcript already pushed only on success, memories, skills) is never corrupted. The turn is retried or reported.
- **State push failure on dehydrate:** treated as a turn failure; the prior durable state remains the source of truth (no partial commit of transcript).
- **Skills publish failure:** reported to the user; the drafted skill remains usable in-session but is not shared until a successful commit.

## Testing Strategy

- **`LocalProcessProvider` parity:** after the Phase 1 extraction, behavior is byte-for-byte equivalent to today; covered by existing channel/cron paths and a provider-level test that the same `TurnJob` yields the same `TurnResult` shape.
- **`StateStore` round-trip:** transcript and `memories/` survive pull → mutate → push → re-pull; memory re-index produces the same search results.
- **`SandboxProvider` contract test:** a shared test suite both `LocalProcessProvider` and `ContainerProvider` must pass (run a turn, resume a session, write a memory, publish a skill).
- **Launcher integration tests:** Fargate and Cloud Run launchers gated behind feature flags / credentials, run in CI against real (or emulated) services.
- **Skills git flow:** draft-in-scratch is usable in-session; `publish` produces a commit at the pinned ref; next turn sees the new skill.

## Rollout Phases

Each phase ships independently and the system keeps working throughout.

1. **Extract `SandboxProvider`**; reimplement today's behavior as `LocalProcessProvider`. No behavior change — pure refactor.
2. **Add `StateStore`** (S3/GCS) + hydrate/dehydrate; prove transcript + `memories/` round-trip while the local provider still runs the subprocess.
3. **Build the worker container image + `ContainerProvider`** with one launcher (AWS *or* GCP first); wire scale-to-zero; migrate skills to the git-backed read-only model + `publish_skill`.
4. **Add the second cloud's launcher** (trivial once the trait exists).

## Open Questions / Future Work

- Warm session-affinity workers (Approach B) to cut per-turn cold-start latency for active conversations.
- Opt-in durable per-user workspace for long-lived project directories (attach-on-demand, kept off the default path to preserve portability).
- Whether the session registry needs to move to Postgres (only if a single router instance is outgrown).
- `publish_skill` review gate: direct commit vs. always-PR — start with direct commit; PR-with-approval is a config toggle if desired.
