# Architecture

Cica runs in two modes from one binary:

- **Single-box** — one process. Channels feed an in-process agent; sessions, memory, and skills live on local disk. No state store required.
- **Cloud** — a long-lived **router** plus a fleet of **ephemeral workers**, coordinated through a shared **state store**. The router is the brain; workers are disposable hands.

The same code runs both ways. Cloud mode is what the rest of this document explains; single-box is the degenerate case where the router and worker are the same process and the store is absent.

## The two roles

**Router (brain).** A long-lived process (`cica`, no subcommand). It:

- Listens on channels (Telegram / Signal / Slack) and debounces incoming messages per user.
- Builds each turn's **system prompt** (identity, user profile, persona, skills, memory).
- Hosts the **memory index** (SQLite + vector search) and runs semantic recall when building a prompt.
- Runs the **skills git-sync loop** — periodically pulls a skills repo and mirrors it to the store.
- Runs the **cron scheduler** for scheduled jobs.
- Dispatches each turn to a worker and returns the reply to the channel.

**Worker (hands).** A one-shot process (`cica worker --turn <id>`). It:

- Reads a `TurnJob` from the store.
- **Hydrates** the session, memory, and skills it needs from the store.
- Runs exactly one agent turn in a sandbox (the agent can run commands, read/write files, use tools).
- **Dehydrates** — writes the updated session and memory back to the store.
- Writes a `TurnResult` to the store and exits.

Workers hold no durable state. Anything that must survive a turn travels through the store.

## A turn, end to end

```
 Channel        Router                         Store (S3/fs)              Worker
   │              │                               │                         │
   │── message ──▶│                               │                         │
   │              │ build system prompt           │                         │
   │              │ (identity/user/persona/        │                        │
   │              │  skills + memory search)      │                         │
   │              │── write TurnJob ─────────────▶│ turns/<id>/job          │
   │              │── launch worker (turn=<id>) ──────────────────────────▶ │
   │              │                               │◀── pull session/<sid> ──│  hydrate
   │              │                               │◀── pull mem/<ch>_<uid> ─│
   │              │                               │◀── pull skills ─────────│
   │              │                               │                         │  run agent turn
   │              │                               │── push session/<sid> ──▶│  dehydrate
   │              │                               │── push mem/<ch>_<uid> ─▶│
   │              │                               │ turns/<id>/result ◀─────│  write result
   │              │◀── poll TurnResult ───────────│                         │  exit
   │◀── reply ────│                               │                         │
   │              │ pull mem/<ch>_<uid>,          │                         │
   │              │ reindex memory (post-turn)    │                         │
```

The router writes the job, launches the worker, and polls for the result. The worker does the hydrate → run → dehydrate cycle. After the reply is sent, the router pulls the (possibly updated) memory and re-indexes it so it's searchable next turn.

## Providers — where a turn executes

The router selects an execution **provider** via `[deployment].provider`:

| Provider | Where the turn runs | Needs a store? | Notes |
|---|---|---|---|
| `local` (default) | In-process | Optional | Single-box. With a store, it's wrapped so sessions/memory persist; without one, pure local. |
| `subprocess` | A forked `cica worker` child process | Yes | Same machine, separate process per turn. |
| `docker` | A Docker container per turn | Yes | Image from `docker_image` (default `cica-worker:latest`). |
| `fargate` | An ECS Fargate task per turn | Yes | Build with `--features fargate`. Settings under `[deployment.fargate]`. |

`subprocess`, `docker`, and `fargate` all use the same dispatch pattern: serialize a `TurnJob` to the store, launch `cica worker --turn <id>`, poll for the `TurnResult`. They differ only in *how* the worker process is launched. If the configured provider can't be built, the router logs the error and falls back to in-process so it still starts.

## The state store

A three-method trait (`StateStore`): `pull(key, dest) → bool`, `push(src, key)`, and `delete(key)`. A key maps to a directory tree; keys use `/` as a namespace separator. A push replaces the stored tree as a whole. Pushing an empty directory stores a present, empty tree rather than deleting the key; deletion is explicit.

- **Filesystem** — keys are directories under a root path. Good for single-box-with-persistence and local testing.
- **S3** — keys are object prefixes in a bucket (behind the `s3` feature). Credentials come from the standard AWS provider chain (env / instance role), **never** from config.

S3 stores each tree as immutable objects under `<prefix>/<key>/gen/<uuid>/<relative-path>` and commits it by writing a JSON manifest at `<prefix>/<key>/current` last. Pulls follow that manifest, so readers see either the complete previous generation or the complete new one. Legacy flat objects under `<prefix>/<key>/` remain readable until the next push migrates and prunes them.
Old generations are pruned only once they are an hour old, so concurrent pushes cannot delete each other's live tree.

### Key layout

| Key | Written by | Read by | Contents |
|---|---|---|---|
| `turns/<turn_id>/job` | Router | Worker | The serialized `TurnJob` (prompt, user, backend, resume id). |
| `turns/<turn_id>/result` | Worker | Router | The serialized `TurnResult` (reply, session id, cost). |
| `session/<backend_session_id>` | Worker | Worker | The agent's session transcript/artifacts, for resuming a conversation. |
| `mem/<channel>_<user_id>` | Worker | Worker + Router | A user's memory markdown files. |
| `skills` | Router (sync loop) | Worker | The published skills tree, mirrored from the skills repo. |

## Hydrate / dehydrate

`HydratingProvider` wraps any inner provider and runs on the worker. Per turn:

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
