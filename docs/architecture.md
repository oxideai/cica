# Architecture

Cica runs in two modes from one binary:

- **Single-box** — one process. Channels feed an in-process agent; sessions, memory, and skills live on local disk. No state store required.
- **Cloud** — a long-lived **router** plus a fleet of **ephemeral workers**, coordinated through a shared **state store**. The router is the brain; workers are disposable hands.

The same code runs both ways. Cloud mode is what the rest of this document explains; single-box is the degenerate case where the router and worker are the same process and the store is absent.

## The two roles

**Router (brain).** A long-lived process (`cica`, no subcommand). It:

- Listens on channels (Telegram / Signal / Slack / Linear) and debounces incoming messages per user.
- Builds each turn's **system prompt** (identity, user profile, persona, skills, memory).
- Hosts the **memory index** (SQLite + vector search) and runs semantic recall when building a prompt.
- Runs the **skills git-sync loop** — periodically pulls a skills repo and mirrors it to the store.
- Runs the **cron scheduler** for scheduled jobs.
- Dispatches each turn to a worker and returns the reply to the channel.

> **Inbound vs outbound.** Telegram long-polls, Slack uses Socket Mode and Signal
> talks to a local daemon — those channels only ever dial *out*. Linear is the
> exception: it POSTs a webhook, so enabling `[channels.linear]` opens a
> listening port on the router. cica does not terminate TLS (put an ALB or a
> reverse proxy in front) but it always verifies the webhook HMAC itself, and
> refuses to start without a signing secret.

**Worker (hands).** A session-affine process (`cica worker --session <affinity_id> ...`). It:

- Reads a `TurnJob` from the store.
- **Hydrates** the session, memory, and skills it needs from the store.
- Runs assigned agent turns serially in a sandbox.
- **Dehydrates** — writes the updated session and memory back to the store.
- Writes each `TurnResult` to the store, then waits for another assignment until it drains.

Workers cache skills and owned backend sessions, but hold no durable state. Anything that must survive a worker exit travels through the store.

## A turn, end to end

```
 Channel        Router                         Store (S3/fs)              Worker
   │              │                               │                         │
   │── message ──▶│                               │                         │
   │              │ build system prompt           │                         │
   │              │ (identity/user/persona/        │                        │
   │              │  skills + memory search)      │                         │
   │              │── owner launch intent ───────▶│ sessions/<a>/owner      │
   │              │── start worker if needed ─────────────────────────────▶ │
   │              │── write job + inbox ─────────▶│ turns/<id>/job          │
   │              │                               │◀── pull session/<sid> ──│  hydrate
   │              │                               │◀── pull mem/<ch>_<uid> ─│
   │              │                               │◀── pull changed skills ─│
   │              │                               │                         │  run agent turn
   │              │                               │── push session/<sid> ──▶│  dehydrate
   │              │                               │── push mem/<ch>_<uid> ─▶│
   │              │                               │ turns/<id>/result ◀─────│  write result
   │              │◀── poll result + heartbeat ───│                         │  wait
   │◀── reply ────│                               │                         │
   │              │ pull mem/<ch>_<uid>,          │                         │
   │              │ reindex memory (post-turn)    │                         │
```

The router reuses a live worker for the affinity or launches one after recording its intent. The worker does the hydrate → run → dehydrate cycle for each assignment. After the reply is sent, the router pulls the updated memory and re-indexes it so it is searchable next turn.

## Providers — where a turn executes

The router selects an execution **provider** via `[deployment].provider`:

| Provider | Where the turn runs | Needs a store? | Notes |
|---|---|---|---|
| `local` (default) | In-process | Optional | Single-box. With a store, it's wrapped so sessions/memory persist; without one, pure local. |
| `subprocess` | A forked `cica worker` child process | Yes | Same machine, one warm process per active affinity. |
| `docker` | A Docker container | Yes | One warm container per active affinity. |
| `fargate` | An ECS Fargate task | Yes | One warm task per active affinity. Build with `--features fargate`. |

`subprocess`, `docker`, and `fargate` use the same warm lifecycle. The router records launch intent before starting a worker, assigns turns through its inbox, and polls point records for results and heartbeat sequence changes. A worker drains after the idle or maximum-age limit. A vanished worker fails its assigned turn; the router never redispatches it automatically.

Deploy the worker image before the router so the router never targets an older worker command contract.

Owner and heartbeat records carry the protocol version and timing-policy hash. Missing or mismatched values make a worker incompatible: the router stops it before launching a replacement. Dropping a dispatch writes `turns/<turn_id>/cancel`; the worker aborts that turn, discards its local backend session artifacts, and remains ready for the next turn.

## The state store

`StateStore` supports atomic directory trees through `pull`, `push`, and `delete`, plus small point records through `get_record`, `put_record`, and `delete_record`. Point-record operations address one plain filesystem file or one S3 object at `<prefix>/<key>` and never list objects. Worker results use a point record at `turns/<turn_id>/result` containing a versioned envelope whose turn and affinity identities are validated before the router accepts the enclosed result or error; S3 deployments should apply a lifecycle expiry to `turns/` as a leak backstop.

- **Filesystem** — keys are directories under a root path. Good for single-box-with-persistence and local testing.
- **S3** — keys are object prefixes in a bucket (behind the `s3` feature). Credentials come from the standard AWS provider chain (env / instance role), **never** from config.

S3 stores each tree as immutable objects under `<prefix>/<key>/gen/<uuid>/<relative-path>` and commits it by writing a JSON manifest at `<prefix>/<key>/current` last. Pulls follow that manifest, so readers see either the complete previous generation or the complete new one. Legacy flat objects under `<prefix>/<key>/` remain readable until the next push migrates and prunes them.
Old generations are pruned only once they are an hour old, so concurrent pushes cannot delete each other's live tree.

### Key layout

| Key | Written by | Read by | Contents |
|---|---|---|---|
| `turns/<turn_id>/job` | Router | Worker | The serialized `TurnJob` (prompt, user, backend, resume id). |
| `turns/<turn_id>/result` | Worker | Router | The versioned result or error envelope. |
| `sessions/<affinity_id>/owner` | Router | Router | Launch phase, worker identity, platform handle, policy and affinity. |
| `sessions/<affinity_id>/inbox` | Router | Worker | The current turn and addressed worker. |
| `sessions/<affinity_id>/workers/<worker_id>` | Worker | Router | Sequence heartbeat, lifecycle phase and current/last turn. |
| `turns/<turn_id>/cancel` | Router | Worker | Per-turn cancellation marker. |
| `skills/head` | Skills sync | Worker | Version changed after each successful skills swap. |
| `session/<backend_session_id>` | Worker | Worker | The agent's session transcript/artifacts, for resuming a conversation. |
| `mem/<channel>_<user_id>` | Worker | Worker + Router | A user's memory markdown files. |
| `skills` | Router (sync loop) | Worker | The published skills tree, mirrored from the skills repo. |

Cloud Run support remains gated until its GCS store passes the state-store contract suite and its launcher implements start, status, stop-and-wait, and reconciliation with one task and no automatic retries. Its platform timeout must exceed `worker_max_age_secs + turn_timeout_secs + 30` seconds.

## Hydrate / dehydrate

Local mode keeps `HydratingProvider` and its per-turn pull behavior. Warm workers use `WarmHydratingProvider`: skills are pulled only when `skills/head` changes, a backend session already produced by that worker stays local, and user memories are pulled every turn because they are shared across affinities.

1. **Hydrate** — if the job names a `resume_session`, pull `session/<id>` and restore it into the backend's home (e.g. `.claude/projects/<slug>/<id>.jsonl`). Then pull `mem/<channel>_<user_id>` (the user's memories) and `skills` (the published corpus) into the working directory.
2. **Run** — delegate to the inner provider (the actual agent invocation).
3. **Dehydrate (best-effort)** — capture the resulting session artifacts and push to `session/<id>`; push updated memories to `mem/<channel>_<user_id>`.

If a state pull fails, hydration logs the error and runs the turn without that state. A key that failed to pull is not pushed back during dehydration.

Dehydration is best-effort: the worker returns the reply to the router *before* persisting, so a slow or failed push degrades resume quality but never drops the answer.

## Skills

Skills are folders under `skills/`. A directory containing a `SKILL.md` is a leaf skill; `node_modules`, `docs`, and hidden dirs are skipped. Each `SKILL.md` has frontmatter:

- `name` (must match the directory), `description`, `when_to_use`
- `category` — one of `tool`, `workflow`, `report`, `knowledge` (default `tool`)

Discovered skills are rendered into the system prompt as XML, grouped by category.

**Git-sync (cloud).** When `[skills]` is configured, the router runs a sync loop: on startup and every `refresh_secs`, it shallow-clones `repo` at `ref`, strips `.git`, pushes the tree to the store under `skills`, then atomically swaps it into the local skills dir. The last-good tree is preserved on any failure. The git credential is read from the `CICA_SKILLS_GIT_TOKEN` environment variable — **never** from config. Workers hydrate the `skills` key each turn, so a sync on the router propagates to the whole fleet.

This decouples the skill corpus from the binary: update skills by pushing to the repo, no redeploy.

## Memory

Each user has memory files under `users/<channel>_<user_id>/memories/`. They're chunked, embedded (a local sentence-embedding model), and indexed in SQLite with vector search. When building a prompt, the router runs a semantic search over the user's memories and injects the most relevant chunks.

**Write-back in cloud mode.** The agent runs on a worker, but the prompt is built on the router — which doesn't know the worker's local path. So the prompt emits a `{MEMORIES_DIR}` token, and the worker's local provider substitutes it for the real per-user path at run time, so files the agent writes land exactly where the worker captures and pushes them to the store.

After the reply is sent, the router's post-turn hook **pulls `mem/<channel>_<user_id>` from the store before re-indexing** — so a memory written on a worker this turn is searchable from the router next turn. In single-box mode there's no store, so the pull is skipped and the router just re-indexes local disk. In cloud mode the store is the source of truth for memory: a pull overwrites the router's local copy, so operator edits should go through a turn or be written to the store directly.

## Channels and onboarding

Channels (`telegram`, `signal`, `slack`) implement a common `Channel` trait (send message, send with attachments, typing indicator). Per-user message handling debounces rapid messages and aborts an in-flight turn when a newer message arrives. The agent's output can carry `[attachment:/path]` markers, which are stripped from the text and sent as native media.

New users go through a pairing flow (auto-approved when `auto_approve` is set, otherwise approved from the host via `cica approve`). Onboarding then runs in two phases — the agent learns its identity (`IDENTITY.md`) and learns about the user (`USER.md`) — unless `shared_identity` is set, in which case a shared `PERSONA.md` is used instead of per-user identity.

## Single-box vs. cloud at a glance

| | Single-box | Cloud |
|---|---|---|
| Processes | One | Router + N ephemeral workers |
| `provider` | `local` (or unset) | `subprocess` / `docker` / `fargate` |
| `store` | Optional | Required |
| Skills | Local folder | Git-synced via the store |
| Memory | Local index | Worker writes → store → router pulls + reindexes |
| Sandbox isolation | None (runs on your box) | Per-turn container/task |

See [configuration.md](configuration.md) for the config that selects each mode.
