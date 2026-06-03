# Docker-Flow CI Test (fake backend) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Docker worker flow automatically testable — with no Cursor/Claude creds, channel, or network — by adding a fake-backend hook and a CI-gated integration test that runs a real turn through the container and asserts the store round-trip. Folds into the Phase 3b-1 branch (`feat/phase3b1-container-worker`, PR #10).

**Architecture:** `backends::query_with_options` short-circuits to a deterministic result when `CICA_FAKE_BACKEND` is set (inert otherwise). `DockerLauncher` gains env passthrough so the launcher can inject that env into the container. A runtime-gated `#[cfg(test)]` test (skipped unless `CICA_DOCKER_IT=1`) drives `LaunchedWorkerProvider` + `DockerLauncher` against the real `cica-worker:latest` image + a tempdir filesystem store, asserting the result round-trips. A CI job builds the image and runs that gated test.

**Tech Stack:** Rust 2024, `tokio::process`, `tempfile` (dev-dep), Docker (CI). No new Rust dependencies.

---

## Why this is safe

The fake-backend hook is a single env-gated branch, inert in normal operation (and harmless if set — it only swaps the CLI call for a canned echo). `DockerLauncher::new`'s signature is unchanged (env added via a builder, default empty), so existing callers/tests are untouched. The integration test returns early unless `CICA_DOCKER_IT=1`, so normal `cargo test` is unaffected. cica is binary-only (no `[lib]`), so the integration test lives in `src/` (a `tests/` file couldn't import internal types).

## Background facts (verified)

- `src/backends/mod.rs`: `pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<QueryResult>` starts with `let config = Config::load()?; match config.backend { ... }`. `QueryResult { response: String, session_id: String, duration_ms: Option<u64>, cost_usd: Option<f64> }`.
- `src/sandbox/worker.rs`: `DockerLauncher::new(image: String, config_file: PathBuf, skills_dir: PathBuf, state_store_dir: PathBuf)`; `run_args(&self, turn_id) -> Vec<String>` builds `["run","--rm","-v",<cfg>,"-v",<skills>,"-v",<store>, image, "worker","--turn", id]`. `LaunchedWorkerProvider::new(store: Arc<dyn StateStore>, launcher: Box<dyn Launcher>)`. `FilesystemStateStore` is imported in the test module.
- `cmd::worker::run` (in-container entrypoint) does `Config::load()` + `default_store` then runs `HydratingProvider`. With `resume_session: None` it skips hydration; the fake backend returns `session_id: ""` so dehydrate skips session capture; a non-existent memories dir skips memory push — so a turn round-trips cleanly with no real backend.
- `.github/workflows/ci.yml`: a `check` job (fmt/clippy/`cargo test`) + a `build` matrix job. Runners are `ubuntu-latest` (Docker available).

## File structure

- Modify `src/backends/mod.rs` — `fake_result` fn + the `CICA_FAKE_BACKEND` short-circuit in `query_with_options`.
- Modify `src/sandbox/worker.rs` — `DockerLauncher` gains an `env` field + `with_env` builder; `run_args` emits `-e`; the gated Docker integration test.
- Modify `.github/workflows/ci.yml` — a `docker-flow` job.

---

### Task 1: Fake backend hook

**Files:**
- Modify: `src/backends/mod.rs`

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)] mod tests` block at the end of `src/backends/mod.rs` (or extend an existing one):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_result_echoes_prompt() {
        let r = fake_result("ping");
        assert_eq!(r.response, "fake-response: ping");
        assert_eq!(r.session_id, "");
        assert_eq!(r.cost_usd, None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test backends::tests::fake_result_echoes_prompt`
Expected: FAIL to compile — `fake_result` not found.

- [ ] **Step 3: Implement the hook**

In `src/backends/mod.rs`, add the helper (module level, above `query_with_options`):

```rust
/// Deterministic stand-in for a real backend response. Used by the Docker
/// integration test (activated via the `CICA_FAKE_BACKEND` env var) to exercise
/// the worker/dispatch pipeline without calling Cursor/Claude.
fn fake_result(prompt: &str) -> QueryResult {
    QueryResult {
        response: format!("fake-response: {prompt}"),
        session_id: String::new(),
        duration_ms: Some(0),
        cost_usd: None,
    }
}
```

Add the short-circuit as the FIRST statement of `query_with_options` (before `Config::load()`):

```rust
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<QueryResult> {
    // Test hook: a deterministic response without invoking the real backend CLI.
    // Inert unless `CICA_FAKE_BACKEND` is set (used only by the Docker CI test).
    if std::env::var_os("CICA_FAKE_BACKEND").is_some() {
        return Ok(fake_result(prompt));
    }

    let config = Config::load()?;
    // ... existing match config.backend { ... } unchanged ...
}
```

(`options` remains used by the real path below; no unused-variable warning.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test backends::tests::fake_result_echoes_prompt`
Expected: PASS. `cargo build` succeeds.

- [ ] **Step 5: Commit**

```bash
git add src/backends/mod.rs
git commit -m "feat(backends): add CICA_FAKE_BACKEND test hook for the docker flow"
```
End every commit with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: `DockerLauncher` env passthrough

**Files:**
- Modify: `src/sandbox/worker.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/sandbox/worker.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn docker_launcher_passes_env() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/c"),
            std::path::PathBuf::from("/s"),
            std::path::PathBuf::from("/st"),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);
        let args = l.run_args("t1");
        // `-e CICA_FAKE_BACKEND=echo` present, before the image
        let e = args.iter().position(|a| a == "-e").unwrap();
        assert_eq!(args[e + 1], "CICA_FAKE_BACKEND=echo");
        let img = args.iter().position(|a| a == "cica-worker:latest").unwrap();
        assert!(e < img);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sandbox::worker::tests::docker_launcher_passes_env`
Expected: FAIL to compile — no `with_env`.

- [ ] **Step 3: Add the `env` field + builder, and emit `-e` in `run_args`**

Change the `DockerLauncher` struct to add an `env` field:

```rust
pub struct DockerLauncher {
    image: String,
    config_file: PathBuf,
    skills_dir: PathBuf,
    state_store_dir: PathBuf,
    env: Vec<(String, String)>,
}
```

In `impl DockerLauncher`, set `env: Vec::new()` in `new` and add the builder:

```rust
    pub fn new(
        image: String,
        config_file: PathBuf,
        skills_dir: PathBuf,
        state_store_dir: PathBuf,
    ) -> Self {
        Self {
            image,
            config_file,
            skills_dir,
            state_store_dir,
            env: Vec::new(),
        }
    }

    /// Extra `-e KEY=VALUE` env vars to pass into the container.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }
```

Rewrite `run_args` to emit the env before the mounts:

```rust
    /// The `docker` argv (without the leading `docker`). Pure, for testing.
    fn run_args(&self, turn_id: &str) -> Vec<String> {
        let mut args = vec!["run".into(), "--rm".into()];
        for (k, v) in &self.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push("-v".into());
        args.push(format!("{}:/data/cica/config.toml:ro", self.config_file.display()));
        args.push("-v".into());
        args.push(format!("{}:/data/cica/skills:ro", self.skills_dir.display()));
        args.push("-v".into());
        args.push(format!("{}:/data/cica/internal/state-store", self.state_store_dir.display()));
        args.push(self.image.clone());
        args.push("worker".into());
        args.push("--turn".into());
        args.push(turn_id.into());
        args
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test sandbox::worker::tests::docker_launcher`
Expected: PASS — both `docker_launcher_builds_run_args` (the existing no-env test still holds: empty env → no `-e`) and the new `docker_launcher_passes_env`. `cargo build` succeeds (`default_provider`'s `DockerLauncher::new(...)` call is unchanged — 4 args).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/worker.rs
git commit -m "feat(sandbox): DockerLauncher env passthrough (-e KEY=VALUE)"
```

---

### Task 3: Gated Docker integration test

**Files:**
- Modify: `src/sandbox/worker.rs`

- [ ] **Step 1: Add the gated test**

Add to `src/sandbox/worker.rs`'s `#[cfg(test)] mod tests`. It returns early unless `CICA_DOCKER_IT=1`, so it's a no-op in normal `cargo test`; the CI `docker-flow` job sets the env after building the image.

```rust
    /// End-to-end Docker flow with the fake backend. Gated: only runs when
    /// `CICA_DOCKER_IT=1` (the CI docker-flow job, after building the image).
    /// Drives the real `cica-worker:latest` container + a tempdir filesystem
    /// store, asserting the turn round-trips with no real backend.
    #[tokio::test]
    async fn docker_flow_round_trips_with_fake_backend() {
        if std::env::var_os("CICA_DOCKER_IT").is_none() {
            return; // skipped unless explicitly enabled
        }

        use crate::config::AiBackend;

        let store_root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // Minimal config.toml to mount (backend is irrelevant — the fake hook
        // short-circuits before the real CLI call).
        let cfg_dir = tempfile::tempdir().unwrap();
        let config_file = cfg_dir.path().join("config.toml");
        std::fs::write(
            &config_file,
            "backend = \"cursor\"\n[deployment]\nstore = \"filesystem\"\n",
        )
        .unwrap();
        let skills_dir = tempfile::tempdir().unwrap();

        let launcher = DockerLauncher::new(
            "cica-worker:latest".into(),
            config_file,
            skills_dir.path().to_path_buf(),
            store_root.path().to_path_buf(),
        )
        .with_env(vec![("CICA_FAKE_BACKEND".into(), "echo".into())]);

        let provider = LaunchedWorkerProvider::new(store.clone(), Box::new(launcher));
        let job = TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "ping".into(),
            system_prompt: None,
            resume_session: None,
            cwd: None,
            skip_permissions: true,
            backend: AiBackend::Cursor,
            model: None,
        };

        let result = provider.run_turn(job).await.expect("docker turn failed");
        assert!(
            result.response.contains("fake-response: ping"),
            "unexpected response: {}",
            result.response
        );
    }
```

- [ ] **Step 2: Verify it's a no-op without the env**

Run: `cargo test sandbox::worker::tests::docker_flow_round_trips_with_fake_backend`
Expected: PASS (returns early — `CICA_DOCKER_IT` is unset). `cargo build` succeeds.

- [ ] **Step 3: (Optional) run it for real if Docker + image are available**

If you have Docker and built the image (`docker build -t cica-worker:latest .`):
Run: `CICA_DOCKER_IT=1 cargo test sandbox::worker::tests::docker_flow_round_trips_with_fake_backend -- --nocapture`
Expected: PASS — a `cica-worker` container runs the turn and the result round-trips `"fake-response: ping"`. (If Docker isn't available here, skip; the CI job covers it.)

- [ ] **Step 4: Commit**

```bash
git add src/sandbox/worker.rs
git commit -m "test(sandbox): gated Docker integration test (fake backend round-trip)"
```

---

### Task 4: CI `docker-flow` job + final gate

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the `docker-flow` job**

In `.github/workflows/ci.yml`, add a new job under `jobs:` (alongside `check` and `build`):

```yaml
  docker-flow:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build worker image
        run: docker build -t cica-worker:latest .

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Docker integration test (fake backend)
        run: CICA_DOCKER_IT=1 cargo test sandbox::worker::tests::docker_flow_round_trips_with_fake_backend -- --nocapture
```

(Keep YAML indentation consistent with the existing `check`/`build` jobs — two-space indent under `jobs:`.)

- [ ] **Step 2: Validate the workflow YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"`
Expected: `yaml ok`.

- [ ] **Step 3: Lint + format + full test (the existing gates)**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS, no warnings.
Run: `cargo fmt` then `cargo fmt --check`
Expected: clean.
Run: `cargo test`
Expected: all pass (the gated docker test is a no-op without the env).

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "ci: build worker image and run the docker integration test"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- Fake backend hook (`CICA_FAKE_BACKEND`) returning a deterministic result → Task 1.
- `DockerLauncher` env passthrough → Task 2.
- Gated Docker integration test driving `LaunchedWorkerProvider` + `DockerLauncher` + fake backend, asserting store round-trip → Task 3.
- CI job: build image + run gated test → Task 4.
- No creds/channel/network needed; binary-crate constraint handled (test in `src/`, runtime-gated) → Task 3.
- Backend-specific `--resume` deliberately NOT covered (already validated manually) → not in scope.

**Placeholder scan:** No "TBD"/"handle errors appropriately". Every code step shows complete code. The "optional run for real" (Task 3 Step 3) is explicit conditional guidance, not a placeholder.

**Type consistency:** `fake_result(prompt: &str) -> QueryResult` matches `QueryResult`'s fields (Task 1). `DockerLauncher::new(image, config_file, skills_dir, state_store_dir)` is unchanged (4 args) so `default_provider` and the existing `docker_launcher_builds_run_args` test still compile; `with_env(Vec<(String,String)>)` is consistent across Tasks 2–3. `run_args` emits `-e KEY=VALUE` (Task 2) asserted by both the env test (Task 2) and exercised by the integration test (Task 3). `LaunchedWorkerProvider::new(store, Box<dyn Launcher>)` + `TurnJob` fields match the existing code.

## Notes

This ships in PR #10 (the Phase 3b-1 branch). The fake backend + `DockerLauncher` env passthrough are also reusable for the 3b-2/3b-3 launcher tests (Fargate/Cloud Run can pass the same fake-backend env to validate dispatch without real backend calls).
