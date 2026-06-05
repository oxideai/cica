# Skills Delivery for Router/Worker Deployments — Design

**Status:** Approved (delivery scope only)
**Date:** 2026-06-05
**Scope:** cica (cloud-agnostic). Deployment wiring (sprout) captured as requirements; implemented separately.

## Problem

In the single-box deployment, skills live in `skills_dir` (`/data/cica/skills`) and the
same process both *lists* them in the prompt (`skills::discover_skills`, router-side) and
*executes* them (the agent reads `SKILL.md` + impl from disk at runtime). The two roles
share one filesystem, so it "just works."

In a router/worker deployment they don't. The router (control plane) builds the prompt and
lists skills with their on-disk paths; the worker (ephemeral, e.g. Fargate) is where the
agent actually runs and tries to *open* those paths. Today workers have **no skills** — the
image bakes none and nothing mounts them — so:

1. Skill **execution** is broken on workers (the listed paths don't exist there).
2. There's no way to **refresh** skills without rebuilding/redeploying.

This is not specific to our AWS deployment. Router/worker is cica's generic cloud shape, so
**every** such deployment hits it, and any solution must be **cloud-agnostic** (S3 today, GCS
or others later). A deployment-level fix (ECS sidecar + EFS mount + a host cron) is
AWS-specific and would have to be re-solved per cloud. Therefore this is solved **in cica**,
on the abstractions cica already has.

## Goals

- Skills are pulled from a git repo (e.g. `github.com/root-global/ai-skills`).
- The router and the workers stay **in sync** about which skills exist.
- Skills **refresh periodically** — no redeploy, no image rebuild.
- The git credential **never reaches workers**.
- **Cloud-agnostic** — no AWS/GCP-specific code in cica.

## Non-Goals (deferred)

- **Draft survival** — an in-progress skill edited across messages surviving the ephemeral
  worker. Separate future phase.
- **Publish-as-PR** — the agent opening a PR to the skills repo. Separate future phase.

This design is **delivery only**: published, read-only skills onto router + workers.

## Architecture

The design leans entirely on two seams cica already has:

- **`StateStore`** — a generic prefix→directory sync (`push(local_dir, key)` /
  `pull(key, local_dir)`), backed by S3 today, any object store later. It already carries
  sessions (`session/<id>`) and memories. Skills become one more prefix: `"skills"`. No new
  trait methods.
- **The worker `HydratingProvider`** — already pulls session + memories before a turn and
  pushes results after. Skills hydration is one more pull (read-only; no push-back).

Flow:

```
            ┌─────────────── router (control plane, always-on) ───────────────┐
  git repo  │  every refresh_secs:                                            │
 (ai-skills)│    git clone <ref> → temp → atomic swap into skills_dir         │
     │      │                              │                                  │
     └──────┼──► (skills_dir read by discover_skills for the prompt)          │
            │                              │                                  │
            │                              └─► store.push(skills_dir,"skills")│
            └──────────────────────────────────────────│────────────────────┘
                                                        ▼
                                                  StateStore (S3/GCS/…)
                                                   prefix "skills"
                                                        │
            ┌────────────── worker (ephemeral) ─────────┼───────────────────┐
            │  HydratingProvider, before the turn:      ▼                   │
            │    store.pull("skills", skills_dir)  ──► agent reads/executes │
            └──────────────────────────────────────────────────────────────┘
```

The router is the only git puller (it holds the credential and is always-on); workers only
read the object store. The router lists exactly what it pushed, and workers hydrate exactly
that tree — so listing and execution agree (cwd is `/data/cica` on both, so the listed paths
resolve in the worker).

## Components

### 1. Config — `[skills]` (router-only)

New optional section in `config.toml`:

```toml
[skills]
repo = "https://github.com/root-global/ai-skills"
ref  = "main"          # branch, tag, or sha
refresh_secs = 600     # router re-pulls every 10 minutes
```

- Parsed into an `Option<SkillsConfig>` on `Config`. Absent → the whole mechanism is
  dormant; cica behaves exactly as today.
- The git credential is **not** in config. It comes from the env var
  `CICA_SKILLS_GIT_TOKEN`, read at pull time. (Keeping it out of `config.toml`/the
  `StateStore` is deliberate — same posture as the AI keys.)
- **Worker config is untouched.** The worker never reads `[skills]` and never needs the
  token — it only needs its existing `StateStore`. Skills hydration is unconditional and
  key-driven.

### 2. Router — periodic git-sync task

A `tokio` task spawned by the router run loop when `[skills]` is present. On startup and then
every `refresh_secs`:

1. Shallow-clone `repo` at `ref` into a fresh temp dir.
   - Auth: invoke `git` with `GIT_ASKPASS` set to a tiny helper script that echoes
     `$CICA_SKILLS_GIT_TOKEN`. The token stays in the env only — never in argv, never
     persisted to `.git/config`.
   - Command shape: `git clone --depth 1 --branch <ref> <repo> <tmp>` (with a fallback to
     `clone` + `checkout <sha>` when `ref` is a sha that `--branch` can't take).
2. On success: `store.push(tmp, "skills")`, then **atomically** `rename` `tmp` over
   `skills_dir` (rename within the same filesystem; the previous tree is swapped out in one
   step so concurrent `discover_skills` readers never see a partial tree).
3. On **any** failure (bad credential, network, repo unavailable, non-zero git exit): log a
   warning and **keep the last-good `skills_dir`** untouched. Stale skills beat no skills.

The task is the single source of truth for both the router's own `skills_dir` and the
`"skills"` prefix in the store — they're written together, so they can't diverge.

### 3. Worker — skills hydration in `HydratingProvider`

Before running the turn (alongside the existing session + memories hydration):

```rust
// Best-effort: published skills are read-only; absence is fine.
let _ = self.store.pull("skills", &skills_dir).await;
```

- Read-only: workers never mutate published skills, so there is **no** dehydrate/push-back.
- If the prefix is empty (router hasn't completed a first sync), `pull` is a no-op →
  `skills_dir` empty → that turn simply has no skills (identical to today's behavior).
  Self-heals on the next turn after the router's first sync lands.

`skills_dir` is `config::paths()?.skills_dir` — the same path `discover_skills` and the agent
already use. No path changes anywhere.

## Data Flow (one turn, cloud)

1. Router's sync task has (at some earlier tick) pulled the repo and pushed `"skills"`.
2. A message arrives; the router builds the prompt, `discover_skills` lists `skills_dir`.
3. Router dispatches the `TurnJob` to a worker.
4. Worker's `HydratingProvider` pulls session, memories, **and `"skills"`** into the worker
   filesystem.
5. The agent runs; when it invokes a skill it reads `skills_dir/<skill>/SKILL.md` (+ impl) —
   present because of step 4.
6. Result pushed back via the store (skills are not pushed back).

## Error Handling

| Failure | Behavior |
|---|---|
| Git auth/network/repo failure on the router | Log warning; keep last-good `skills_dir`; retry next interval. |
| `[skills]` unset | Sync task not spawned; cica behaves exactly as today. |
| `CICA_SKILLS_GIT_TOKEN` missing while `[skills]` set | Log a clear error on first sync; keep behaving (no skills) rather than crash. |
| Worker `pull("skills")` finds nothing | No-op; turn proceeds with no skills; self-heals after router's first sync. |
| Partial/interrupted clone | Never swapped in (swap only on success); `skills_dir` stays last-good. |

## Testing

- **Router sync — happy path:** against a temp local git fixture repo + an in-memory/temp
  `StateStore`: run one sync; assert `skills_dir` contains the fixture's skills and that
  `store.pull("skills", …)` returns the same tree.
- **Router sync — failure keeps last-good:** seed `skills_dir` with a known tree; point the
  sync at a bogus repo/ref; assert `skills_dir` is unchanged and the store prefix is
  unchanged.
- **Atomic swap:** assert the temp dir is renamed into place (no partial state) — covered by
  asserting the post-sync tree matches the fixture exactly.
- **Worker hydration:** seed `"skills"` in a temp `StateStore`; run `HydratingProvider` over a
  recording inner provider; assert `skills_dir` is populated before the inner `run_turn` and
  that an empty prefix yields an empty `skills_dir` without error.
- **`discover_skills`** is already covered; no change.

## Deployment Requirements (per cloud; sprout-specific, implemented separately)

cica is cloud-agnostic; each deployment must provide:

1. **`CICA_SKILLS_GIT_TOKEN` on the router** from its secret store. For sprout: a read-only,
   fine-grained GitHub token (repo: `root-global/ai-skills`, contents:read) added to a
   Secrets Manager secret and injected as router env (router stack).
2. **`git` on the router host.** Workers do **not** need git.
3. The router's `config.toml` gains a `[skills]` section (router config on EFS).

Workers need nothing new — they already have a `StateStore` (`CICA_STORE=s3` + bucket/region).
These sprout changes are a small follow-up plan, separate from the cica feature.

## Why not the alternatives

- **Bake skills into the worker image (pinned ref):** fails "refresh periodically" — every
  skill change needs an image rebuild + redeploy.
- **Worker git-clones per task:** puts the git credential in every worker and adds clone
  latency to every cold start; router/worker can drift mid-turn.
- **Deployment-level (ECS sidecar + EFS mount + host cron):** works, but is AWS-specific —
  none of it ports to GCP, so the problem gets re-solved per cloud. The cica-level design
  solves it once for every cloud cica runs on.
