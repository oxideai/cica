# SandboxProvider Extraction (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `SandboxProvider` abstraction and a `LocalProcessProvider` that reproduces today's behavior exactly, routing every agent turn through the provider — a pure, behavior-preserving refactor that creates the seam for later remote-worker phases.

**Architecture:** A new `src/sandbox/` module defines a `SandboxProvider` trait plus `TurnJob`/`TurnResult` value types. `LocalProcessProvider` implements the trait by mapping a `TurnJob` to the existing `backends::QueryOptions`, calling the current `backends::query_with_options`, and mapping the `QueryResult` back to a `TurnResult`. The two call sites that drive agent turns — `query_ai_with_session` and `execute_cron_job` in `src/channels/mod.rs` — are rewired to dispatch through a provider obtained from a factory, instead of calling `backends::query_with_options` directly. No observable behavior changes.

**Tech Stack:** Rust 2024, `tokio`, `async-trait`, `anyhow`. Tests via `cargo test`; lint via `cargo clippy`.

---

## Why this is a safe refactor

The whole turn-dispatch funnels through `backends::query_with_options`, called from exactly two orchestration points:
- `query_ai_with_session` (`src/channels/mod.rs:1008`) — interactive channel turns, including the "expired session → retry fresh" recovery (`src/channels/mod.rs:1029-1073`).
- `execute_cron_job` (`src/channels/mod.rs:955`) — scheduled turns.

We insert the provider *between* these call sites and `backends::query_with_options`. The backend modules (`src/backends/claude.rs`, `src/backends/cursor.rs`) are untouched; `LocalProcessProvider` wraps them. Because the wrapping is field-for-field, the system behaves identically.

## Scope (Phase 1 only)

In scope: trait + value types, `LocalProcessProvider`, provider factory, rewiring the two call sites, unit tests for the pure mappings, build/clippy/test green, manual smoke.

**Out of scope (later phases, do NOT add now — YAGNI):** `StateHandle` / state hydration, object store, the container/Fargate/Cloud Run launchers, git-backed skills, memory changes. `TurnJob` therefore carries only what `LocalProcessProvider` needs today plus cheap identity fields (`channel`, `user_id`, `session_id`) that orient future phases.

## File Structure

- Create: `src/sandbox/mod.rs` — module root: re-exports, `SandboxProvider` trait, `TurnJob`, `TurnResult`, `default_provider` factory.
- Create: `src/sandbox/local.rs` — `LocalProcessProvider` + the pure mapping helpers.
- Modify: `src/main.rs:11` — register `mod sandbox;`.
- Modify: `src/channels/mod.rs` — import `sandbox`; rewire `query_ai_with_session` and `execute_cron_job`.

---

### Task 1: Create the `sandbox` module with value types and trait

**Files:**
- Create: `src/sandbox/mod.rs`
- Modify: `src/main.rs` (add `mod sandbox;`)

- [ ] **Step 1: Register the module**

In `src/main.rs`, add the module declaration in alphabetical position (after `mod pairing;`, before `mod setup;`):

```rust
mod pairing;
mod sandbox;
mod setup;
```

- [ ] **Step 2: Write `src/sandbox/mod.rs` with types, trait, and an object-safety test**

```rust
//! Sandbox abstraction: where an agent turn executes.
//!
//! Phase 1 provides only `LocalProcessProvider`, which runs the agent as a
//! local subprocess (today's behavior). Later phases add container-based
//! providers behind the same `SandboxProvider` trait.

mod local;

pub use local::LocalProcessProvider;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{AiBackend, Config};

/// A single agent turn to execute.
///
/// Phase 1 carries the fields needed to reproduce the current subprocess call,
/// plus cheap identity fields for future phases. State hydration handles are
/// intentionally absent (added in Phase 2).
#[derive(Debug, Clone)]
pub struct TurnJob {
    /// Logical cica session key (e.g. "telegram:123"). Identity only in Phase 1.
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    /// The user/cron prompt to send to the agent.
    pub prompt: String,
    /// System prompt (full on new session, appended on resume — backend decides).
    pub system_prompt: Option<String>,
    /// Backend session id to resume, if any.
    pub resume_session: Option<String>,
    /// Working directory override.
    pub cwd: Option<String>,
    pub skip_permissions: bool,
    /// Which backend runs this turn.
    pub backend: AiBackend,
    /// Model override.
    pub model: Option<String>,
}

/// Result of executing a `TurnJob`.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub response: String,
    /// Backend-assigned session id for the resulting conversation.
    pub backend_session_id: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
}

/// Where an agent turn executes.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult>;
}

/// Build the provider selected by configuration.
///
/// Phase 1 always returns the local provider; later phases branch on config.
pub fn default_provider(_config: &Config) -> Box<dyn SandboxProvider> {
    Box::new(LocalProcessProvider::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time guarantee that the trait stays object-safe (Box<dyn _>).
    fn _assert_object_safe(_p: &dyn SandboxProvider) {}

    #[test]
    fn default_provider_is_constructible() {
        let cfg = Config::default();
        let _p = default_provider(&cfg);
    }
}
```

- [ ] **Step 3: Verify it fails to compile (local module not yet created)**

Run: `cargo build`
Expected: FAIL — `file not found for module \`local\`` (or unresolved `LocalProcessProvider`).

- [ ] **Step 4: (No code change yet — Task 2 creates `local.rs`.)** Proceed to Task 2.

---

### Task 2: Pure mapping `TurnJob` → `backends::QueryOptions`

**Files:**
- Create: `src/sandbox/local.rs`
- Test: `src/sandbox/local.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Create `src/sandbox/local.rs` with only the test and a stub signature:

```rust
//! Local-subprocess sandbox provider (Phase 1: today's behavior).

use crate::backends::{self, QueryResult};
use crate::sandbox::{TurnJob, TurnResult};

/// Map a `TurnJob` to the backend-agnostic `QueryOptions`.
fn job_to_query_options(job: &TurnJob) -> backends::QueryOptions {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiBackend;

    fn sample_job() -> TurnJob {
        TurnJob {
            session_id: "telegram:42".into(),
            channel: "telegram".into(),
            user_id: "42".into(),
            prompt: "hello".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            cwd: Some("/tmp/work".into()),
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: Some("claude-opus-4-6".into()),
        }
    }

    #[test]
    fn job_maps_to_query_options() {
        let job = sample_job();
        let opts = job_to_query_options(&job);
        assert_eq!(opts.system_prompt.as_deref(), Some("ctx"));
        assert_eq!(opts.resume_session.as_deref(), Some("sess-1"));
        assert_eq!(opts.cwd.as_deref(), Some("/tmp/work"));
        assert!(opts.skip_permissions);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib sandbox::local::tests::job_maps_to_query_options`
Expected: PANIC at `todo!()` (`not yet implemented`).

- [ ] **Step 3: Implement the mapping**

Replace the `todo!()` body:

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

> Note: `backends::QueryOptions` (`src/backends/mod.rs:11-17`) has no `model`/`backend` fields — backend selection and model come from `Config` inside `backends::query_with_options`. Phase 1 preserves that exactly: `job.backend`/`job.model` are not forwarded here. (Wiring per-job backend/model into `backends` is deliberately deferred; today both are read from config, and this refactor must not change behavior.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib sandbox::local::tests::job_maps_to_query_options`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/sandbox/mod.rs src/sandbox/local.rs
git commit -m "refactor(sandbox): add SandboxProvider trait, types, and job→options mapping"
```

---

### Task 3: Pure mapping `QueryResult` → `TurnResult` (and back to `QueryResult`)

**Files:**
- Modify: `src/sandbox/local.rs`

The call sites currently consume `QueryResult` (fields `response`, `session_id`, `duration_ms`, `cost_usd` — see `src/backends/mod.rs:20-26`). To keep downstream code unchanged we need a clean conversion both ways: backend `QueryResult` → `TurnResult` (provider output), and `TurnResult` → `QueryResult` (so `query_ai_with_session` can keep returning `QueryResult`).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/sandbox/local.rs`:

```rust
    #[test]
    fn query_result_maps_to_turn_result() {
        let qr = QueryResult {
            response: "hi".into(),
            session_id: "sess-9".into(),
            duration_ms: Some(123),
            cost_usd: Some(0.5),
        };
        let tr = turn_result_from_query(qr);
        assert_eq!(tr.response, "hi");
        assert_eq!(tr.backend_session_id, "sess-9");
        assert_eq!(tr.duration_ms, Some(123));
        assert_eq!(tr.cost_usd, Some(0.5));
    }

    #[test]
    fn turn_result_maps_back_to_query_result() {
        let tr = TurnResult {
            response: "yo".into(),
            backend_session_id: "sess-3".into(),
            cost_usd: None,
            duration_ms: None,
        };
        let qr = query_result_from_turn(tr);
        assert_eq!(qr.response, "yo");
        assert_eq!(qr.session_id, "sess-3");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib sandbox::local::tests::`
Expected: FAIL to compile — `turn_result_from_query` / `query_result_from_turn` not found.

- [ ] **Step 3: Implement the conversions**

Add to `src/sandbox/local.rs` (module level, above `tests`):

```rust
/// Convert a backend `QueryResult` into a `TurnResult`.
pub(crate) fn turn_result_from_query(qr: QueryResult) -> TurnResult {
    TurnResult {
        response: qr.response,
        backend_session_id: qr.session_id,
        cost_usd: qr.cost_usd,
        duration_ms: qr.duration_ms,
    }
}

/// Convert a `TurnResult` back into a `QueryResult` for existing call sites.
pub fn query_result_from_turn(tr: TurnResult) -> QueryResult {
    QueryResult {
        response: tr.response,
        session_id: tr.backend_session_id,
        duration_ms: tr.duration_ms,
        cost_usd: tr.cost_usd,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib sandbox::local::tests::`
Expected: PASS (all three local tests).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/local.rs
git commit -m "refactor(sandbox): add QueryResult<->TurnResult conversions"
```

---

### Task 4: Implement `LocalProcessProvider`

**Files:**
- Modify: `src/sandbox/local.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/sandbox/local.rs`:

```rust
    #[test]
    fn provider_is_constructible_and_object_safe() {
        let p = LocalProcessProvider::new();
        let _boxed: Box<dyn crate::sandbox::SandboxProvider> = Box::new(p);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib sandbox::local::tests::provider_is_constructible_and_object_safe`
Expected: FAIL to compile — `LocalProcessProvider` not found.

- [ ] **Step 3: Implement the provider**

Add to `src/sandbox/local.rs` (module level):

```rust
use anyhow::Result;
use async_trait::async_trait;

use crate::sandbox::SandboxProvider;

/// Runs an agent turn as a local subprocess — the original cica behavior.
#[derive(Default)]
pub struct LocalProcessProvider;

impl LocalProcessProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxProvider for LocalProcessProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let options = job_to_query_options(&job);
        let qr = backends::query_with_options(&job.prompt, options).await?;
        Ok(turn_result_from_query(qr))
    }
}
```

> Ensure the `use` lines added here don't duplicate existing imports at the top of the file. If `anyhow::Result` / `async_trait::async_trait` are already imported, keep a single import each.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib sandbox::local`
Expected: PASS (all local tests).

- [ ] **Step 5: Build the whole crate**

Run: `cargo build`
Expected: SUCCESS (Task 1's `mod.rs` now resolves `LocalProcessProvider`).

- [ ] **Step 6: Commit**

```bash
git add src/sandbox/local.rs
git commit -m "feat(sandbox): implement LocalProcessProvider"
```

---

### Task 5: Rewire `query_ai_with_session` to dispatch through the provider

**Files:**
- Modify: `src/channels/mod.rs` (imports; `query_ai_with_session`, around `src/channels/mod.rs:1008-1085`)

This preserves the exact control flow, including the "expired session → retry fresh" recovery, by calling `provider.run_turn(...)` where it previously called `backends::query_with_options(...)`.

- [ ] **Step 1: Add the import**

Near the existing `use crate::backends::{self, QueryOptions, QueryResult};` (`src/channels/mod.rs:15`), add:

```rust
use crate::sandbox::{self, TurnJob};
```

- [ ] **Step 2: Replace the body of `query_ai_with_session`**

Replace the current implementation (from the `let existing_session = ...` line through the final `Ok(qr)`) with the version below. It builds a `TurnJob`, dispatches through `sandbox::default_provider`, and maps results back via `sandbox::local::query_result_from_turn`.

```rust
    let existing_session = store.sessions.get(&session_key).cloned();

    let config = crate::config::Config::load()?;
    let provider = sandbox::default_provider(&config);

    let job = TurnJob {
        session_id: session_key.clone(),
        channel: channel.to_string(),
        user_id: user_id.to_string(),
        prompt: text.to_string(),
        system_prompt: Some(context_prompt.clone()),
        resume_session: existing_session,
        cwd: None,
        skip_permissions: true,
        backend: config.backend,
        model: None,
    };

    let qr = match provider.run_turn(job).await {
        Ok(tr) => sandbox::local::query_result_from_turn(tr),
        Err(e) => {
            let error_msg = e.to_string();
            // If session not found, clear it and retry without resuming
            if error_msg.contains("No conversation found with session ID")
                || error_msg.contains("session")
            {
                warn!("Session expired, starting fresh conversation");
                store.sessions.remove(&session_key);
                store.save()?;

                audit::log_event("session_expired", Some(channel), Some(user_id), None);

                let retry_job = TurnJob {
                    session_id: session_key.clone(),
                    channel: channel.to_string(),
                    user_id: user_id.to_string(),
                    prompt: text.to_string(),
                    system_prompt: Some(context_prompt),
                    resume_session: None,
                    cwd: None,
                    skip_permissions: true,
                    backend: config.backend,
                    model: None,
                };

                match provider.run_turn(retry_job).await {
                    Ok(tr) => sandbox::local::query_result_from_turn(tr),
                    Err(e) => {
                        warn!("AI backend error on retry: {}", e);
                        QueryResult {
                            response: format!("Sorry, I encountered an error: {}", e),
                            session_id: String::new(),
                            duration_ms: None,
                            cost_usd: None,
                        }
                    }
                }
            } else {
                warn!("AI backend error: {}", e);
                QueryResult {
                    response: format!("Sorry, I encountered an error: {}", e),
                    session_id: String::new(),
                    duration_ms: None,
                    cost_usd: None,
                }
            }
        }
    };

    // Save session ID for future messages
    if !qr.session_id.is_empty()
        && store.sessions.get(&session_key).map(|s| s.as_str()) != Some(&qr.session_id)
    {
        store.sessions.insert(session_key, qr.session_id.clone());
        store.save()?;
    }

    Ok(qr)
}
```

> The function still returns `QueryResult`, so every channel handler that calls `query_ai_with_session` is unchanged. The previous local `let options = ...` block (`src/channels/mod.rs:1022-1027`) is removed because `QueryOptions` is now built inside the provider.

- [ ] **Step 3: Make `query_result_from_turn` reachable**

`query_result_from_turn` lives in `src/sandbox/local.rs`. Confirm `src/sandbox/mod.rs` exposes the `local` module to the crate. In `src/sandbox/mod.rs` change:

```rust
mod local;
```

to:

```rust
pub mod local;
```

(Keep the existing `pub use local::LocalProcessProvider;`.)

- [ ] **Step 4: Build and run the existing test suite**

Run: `cargo build && cargo test`
Expected: SUCCESS; all existing tests still pass. If the compiler warns that `QueryOptions` is now unused in `src/channels/mod.rs`, remove it from the `use crate::backends::{...}` import.

- [ ] **Step 5: Commit**

```bash
git add src/channels/mod.rs src/sandbox/mod.rs
git commit -m "refactor(channels): dispatch interactive turns through SandboxProvider"
```

---

### Task 6: Rewire `execute_cron_job` to dispatch through the provider

**Files:**
- Modify: `src/channels/mod.rs` (`execute_cron_job`, around `src/channels/mod.rs:940-966`)

- [ ] **Step 1: Replace the `backends::query_with_options` call**

In `execute_cron_job`, replace the block:

```rust
    let qr = backends::query_with_options(
        &job.prompt,
        QueryOptions {
            system_prompt: Some(context_prompt),
            skip_permissions: true,
            ..Default::default()
        },
    )
    .await?;

    Ok(format!("[Cron: {}]\n\n{}", job.name, qr.response))
```

with:

```rust
    let config = crate::config::Config::load()?;
    let provider = sandbox::default_provider(&config);

    let turn = TurnJob {
        session_id: format!("{}:{}", channel, user_id),
        channel: channel.to_string(),
        user_id: user_id.to_string(),
        prompt: job.prompt.clone(),
        system_prompt: Some(context_prompt),
        resume_session: None,
        cwd: None,
        skip_permissions: true,
        backend: config.backend,
        model: None,
    };

    let tr = provider.run_turn(turn).await?;

    Ok(format!("[Cron: {}]\n\n{}", job.name, tr.response))
```

> Behavior preserved: cron turns previously passed only `system_prompt` + `skip_permissions` with all else defaulted (no resume, no cwd, no model). The `TurnJob` above matches that exactly.

- [ ] **Step 2: Build and test**

Run: `cargo build && cargo test`
Expected: SUCCESS; all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/channels/mod.rs
git commit -m "refactor(cron): dispatch scheduled turns through SandboxProvider"
```

---

### Task 7: Lint, dead-code sweep, and final verification

**Files:**
- Possibly modify: `src/backends/mod.rs`, `src/channels/mod.rs` (imports only)

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS (no warnings). Common fixes:
- Unused `QueryOptions` import in `src/channels/mod.rs` → remove from the `use` group.
- If `backends::query_with_options` is now only referenced from `src/sandbox/local.rs`, that is correct and expected — do not delete it.

- [ ] **Step 2: Format**

Run: `cargo fmt`
Then: `cargo fmt --check`
Expected: clean.

- [ ] **Step 3: Full test run**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 4: Manual smoke test (requires a configured cica + claude/cursor installed)**

Run: `cargo run -- ` (or the built binary), send a message on a configured channel, and confirm:
- A normal message gets a reply (interactive path through `query_ai_with_session`).
- A second message in the same conversation resumes (session id reused — check logs / `audit`).
- `/cron run <job-id>` produces output (cron path through `execute_cron_job`).

Expected: identical behavior to before this phase. If `claude`/`cursor` is not installed in this environment, document that the manual smoke must be run where it is, and rely on the green unit suite + clippy for the rest.

- [ ] **Step 5: Final commit (if fmt/clippy made changes)**

```bash
git add -A
git commit -m "chore: fmt and clippy for sandbox provider extraction"
```

---

## Self-Review (completed by plan author)

**Spec coverage (Phase 1 portion of the design):**
- "Extract `SandboxProvider`; reimplement today's behavior as `LocalProcessProvider`. No behavior change." → Tasks 1–6.
- The seam at `backends::query_with_options` / `query_ai_with_session` / `execute_cron_job` → Tasks 5–6 rewire both call sites; backends untouched.
- `LocalProcessProvider` as the default that keeps the single-binary path working with zero infra → Task 1 `default_provider`, Task 4 implementation.
- Later-phase fields (`StateHandle`, container launchers, git skills, memory changes) deliberately excluded → documented under Scope.

**Placeholder scan:** No "TBD"/"handle errors appropriately"/"similar to" — every code step shows complete code. The single `todo!()` (Task 2 Step 1) is an intentional failing-test stub, removed in the same task's Step 3.

**Type consistency:** `TurnJob`/`TurnResult` field names are identical across `mod.rs` (Task 1), the mappings (Tasks 2–3), the provider (Task 4), and both call sites (Tasks 5–6). `query_result_from_turn` is named identically everywhere it is referenced. `QueryOptions` is constructed only inside `job_to_query_options`, matching `src/backends/mod.rs:11-17` (no `model`/`backend` fields), consistent with the note in Task 2 Step 3.

## Next Phases (separate plans)

- **Phase 2:** `StateStore` trait (S3/GCS) + hydrate/dehydrate of transcript and `memories/`, still using the local provider.
- **Phase 3:** worker container image + `ContainerProvider` with the first launcher (AWS or GCP); git-backed read-only skills + `publish_skill`; scale-to-zero.
- **Phase 4:** second cloud launcher.
