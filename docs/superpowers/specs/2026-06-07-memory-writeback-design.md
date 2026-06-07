# Per-User Memory Write-Back in the Distributed Deployment (River — E2)

**Goal:** Resurrect cica's per-user memory loop for the Sprout (router + ephemeral worker) era — memories saved on a worker persist via S3 and become searchable from the router on the next turn — and teach the agent to route *personal* facts to its memory vs *org-wide* facts to `propose-knowledge` (the E1 corpus write-back).

**Architecture:** Three targeted fixes in cica, all gated on `[deployment].store` so single-box behavior is unchanged. (1) The system prompt stops embedding a router-absolute memories path and emits a `{MEMORIES_DIR}` token, substituted once at run time to the *local* path of whichever process runs the agent. (2) The router pulls `mem/` from S3 before reindexing in its existing post-turn hook. (3) The `## Memories` guidance is rewritten with the personal-vs-org routing rule.

**Tech stack:** Rust (cica). Reuses the existing `MemoryIndex` (fastembed BGE-small + sqlite-vec), the `StateStore` pull/push, and the `HydratingProvider`/`LocalProcessProvider` flow. No ai-skills or sprout changes.

---

## 1. Context & the breakage

cica has a complete-looking per-user memory system (`src/memory.rs`): markdown files under `users/{channel}_{user_id}/memories/`, chunked + embedded into a sqlite-vec index, semantic-searched at prompt-build time, with the top matches injected into the system prompt. It worked in the single-box era. The move to a distributed router + ephemeral Fargate workers ([[distributed-deployment-architecture]]) broke it in three places:

1. **Save path is router-absolute, agent runs on the worker.** `onboarding.rs` embeds `config::paths()?.base.join(...)` — a *router* path — into the prompt. The agent executes on a worker, so a file written to that path lands on the worker's filesystem (a different machine) and is then captured/pushed by the `HydratingProvider` to S3 *only if it's under the worker's own memories dir* — which it isn't, because the prompt told it the wrong place.
2. **Router never reads memories back from S3.** The `HydratingProvider` pushes `mem/{channel}_{user_id}` to S3 after a worker turn (hydrating.rs:104), but nothing pulls it back to the router, and search runs on the router's local index. So even a correctly-saved memory is invisible to future prompt builds.
3. **Guidance is stale.** The prompt text ("significant life events", personal-assistant framing) predates Sprout-as-work-assistant and the E1 knowledge corpus, and offers no rule for *which* write-back path (personal memory vs `propose-knowledge`) a given fact belongs to.

The fix is to make memory honor the same S3-mediated round-trip the session and skills paths already use, and to update the guidance for the new world.

### Decisions locked in brainstorming
1. **Keep the vector-search design** (do not simplify away embeddings) — memories may grow past what fits in a prompt, semantic recall earns its keep.
2. **Sync timing = pull-after-turn.** The router pulls `mem/` + reindexes in its existing post-turn hook; no S3 round-trip added to the user-facing path. A memory saved this turn is searchable from the next.
3. **Full guidance rewrite** with the personal-vs-org routing rule; keep the ask-first gate for personal memories.

---

## 2. The loop (distributed mode)

1. Router builds the system prompt with a `{MEMORIES_DIR}` token + relevant memory chunks from its local index (search as today).
2. Worker runs the turn. `LocalProcessProvider` substitutes `{MEMORIES_DIR}` → the worker-local memories dir (`/data/cica/users/{channel}_{user_id}/memories`). The agent, if it learns a durable *personal* fact and the user agrees, writes a markdown file there.
3. `HydratingProvider` (worker) dehydrates: pushes that memories dir to `mem/{channel}_{user_id}` in S3 (existing behavior, hydrating.rs:104).
4. Router's post-turn hook (`reindex_user_memories`): **pull** `mem/{channel}_{user_id}` from S3 into the router-local memories dir, **then** reindex into the sqlite-vec DB.
5. Next turn, the new memory is in the index and surfaces in search → injected into the prompt. Loop closed.

Single-box mode: no store configured → no token substitution divergence (worker == router == same paths), no pull step. Identical to today's behavior.

---

## 3. Save-path fix — `{MEMORIES_DIR}` token

- `onboarding::build_context_prompt_for_user` emits the literal token `{MEMORIES_DIR}` everywhere it currently interpolates `memories_dir(ch, uid)` into guidance text. It no longer calls `config::paths()` for that path. (Search injection in §step-1 is unaffected — it reads the local index, not a path string.)
- Substitution happens at exactly **one** site: `LocalProcessProvider::run_turn`, just before invoking the backend. It computes the local memories dir from the job's `channel`/`user_id` via the same helper logic the `HydratingProvider` uses (`paths.base / "users" / "{channel}_{user_id}" / "memories"`) and string-replaces `{MEMORIES_DIR}` in `job.system_prompt`.
  - **Why here:** `LocalProcessProvider` is the process that actually spawns the agent in every deployment — it's the inner provider of `HydratingProvider` on the worker, and the direct provider in single-box mode. Substituting here guarantees the path matches the filesystem the agent writes to and that `HydratingProvider` later captures.
  - Path derivation must agree byte-for-byte with `HydratingProvider::memories_dir` so the written file is under the dir that gets pushed. Factor the `users/{channel}_{user_id}/memories` join into one shared helper to prevent drift.
- The directory is created if absent before the turn (the agent expects to write into it).

## 4. Router read-path fix — pull-before-reindex

- `reindex_user_memories(channel, user_id)` (channels/mod.rs:1031, called post-turn at :389) gains a leading step, gated on a configured store:
  - If `[deployment].store` resolves (`default_store(&config)?` is `Some`), `store.pull(&format!("mem/{channel}_{user_id}"), &memories_dir(channel, user_id))` into the router-local dir, then proceed to open `MemoryIndex` and `index_user_memories` as today.
  - If no store (single-box), skip the pull — local files are already authoritative.
- This function is sync today and called fire-and-forget; the pull is async. Make the hook spawn/await appropriately (it already runs after the reply is sent, so latency here is off the user path). Pull failure → warn and still reindex whatever is local (never block, matching the rest of the pipeline).
- **Source-of-truth note:** in distributed mode S3 becomes authoritative for memories on the router — a pull overwrites local. Operator/manual memory edits must go through a worker turn or be written to S3 directly; hand-edits on the router's disk will be clobbered on the next pull. Document this in the spec and a code comment.

## 5. Guidance rewrite — the routing rule

Replace the `## Memories` block in `onboarding.rs` with work-assistant-era guidance:

- **Personal / user-specific** — preferences, ongoing projects this user is driving, how they like answers, facts they share about themselves: save a memory file at `{MEMORIES_DIR}`. **Ask first**; use a descriptive filename; format with headers/bullets; don't save trivia. (Rules unchanged; flavor text like "significant life events" removed.)
- **Durable org-wide** — where features live, schema/data gotchas, glossary terms, repo-routing rules: do **not** put these in personal memory. Offer `propose-knowledge` instead (a Draft PR to the shared corpus). This is the one decision the agent must get right; state it explicitly so personal memory and the corpus don't blur.

## 6. Error handling
- Pull/reindex failures: warn-and-continue (consistent with hydrating.rs and the existing reindex hook). Never block a reply or crash a turn on a memory-plumbing failure.
- Token left unsubstituted (e.g. a path that bypasses `LocalProcessProvider`): the agent sees a literal `{MEMORIES_DIR}`; harmless (it just won't write there). Acceptable — every real execution path goes through `LocalProcessProvider`.

## 7. Testing approach
- **Unit (LocalProcessProvider):** `{MEMORIES_DIR}` in `job.system_prompt` is replaced with the correct local path derived from `channel`/`user_id`; absent token → prompt unchanged; substituted path equals `HydratingProvider::memories_dir` for the same inputs (shared-helper agreement).
- **Unit (reindex hook):** with a mock store (reuse the in-memory store pattern from hydrating.rs tests) seeded with a `mem/...` blob, `reindex_user_memories` pulls it into the local dir before indexing; with no store, it does not attempt a pull and indexes local files only.
- **Live sign-off:** in a Sprout session, tell it a durable preference → it asks → on yes, confirm a file appears under `mem/{channel}_{user_id}` in S3 (`aws s3 ls`) → start a fresh conversation next turn → it recalls the preference (surfaced via injected search results). Separately, state a durable *org* fact → it offers `propose-knowledge`, not a personal memory.

## 8. Definition of done
- The system prompt emits `{MEMORIES_DIR}`; `LocalProcessProvider` substitutes it to the local memories dir via a helper shared with `HydratingProvider`; unit-tested.
- `reindex_user_memories` pulls `mem/...` from S3 before reindexing when a store is configured; no-op pull in single-box; unit-tested.
- The `## Memories` guidance is rewritten with the personal-vs-org routing rule; ask-first retained for personal memories.
- `cargo build` + `cargo test --bin cica` green; deslop pass clean.
- Live sign-off (§7) passes: a personal memory round-trips worker→S3→router→next-turn recall, and an org fact routes to `propose-knowledge`.

## 9. Out of scope (later)
- Memory editing/deletion/compaction UX (today the agent overwrites files; no GC).
- Cross-user or team-shared personal memory (that's what the corpus is for).
- Dropping/replacing the embedding machinery (BGE + sqlite-vec stays).
- Autonomous (no-ask) memory saves; end-of-session reflection.
- Pre-prompt freshness checks for out-of-band/cron memory writes (chose pull-after-turn).

## 10. Operational
cica-only; one PR → version bump → push `v*` tag → `update-router.sh` (router) + fleet deploy (`-c cicaVersion=x -c efsFileSystemId=fs-05e70fd01174cf53c --exclusively`). See [[river-strategy]] for the full deploy playbook. No ai-skills sync or secret changes required.
