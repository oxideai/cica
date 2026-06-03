# Phase 3b-1: Containerized Worker + Local Docker Launcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run an agent turn inside a one-shot Docker container instead of a subprocess — generalizing 3a's dispatch behind a `Launcher` trait, adding a `DockerLauncher` + worker `Dockerfile`, so we prove the containerized turn round-trip (including fresh-container isolation) locally with no cloud.

**Architecture:** Extract 3a's store-mediated dispatch into a `LaunchedWorkerProvider` that delegates the "run the worker for `turn_id`, await exit" step to a `Launcher`. `SubprocessLauncher` reproduces 3a's behavior; `DockerLauncher` runs the worker image via `docker run` with the host `config.toml`, `skills/`, and state-store bind-mounted into a `/data/cica`-pinned container. Selected by `[deployment] provider = "docker"`.

**Tech Stack:** Rust 2024, `tokio::process`, `async-trait`, `anyhow`, `uuid` (existing). Docker (runtime, shelled out — no Rust SDK). `tempfile` dev-dep for tests. No new Rust dependencies.

---

## Why this is safe and incremental

The refactor (Task 1) is behavior-preserving for `provider = "subprocess"` (3a). Everything else is additive behind `provider = "docker"`. No new Rust deps (DockerLauncher shells out to `docker`). Default `cargo build` and `install.sh` unchanged. Cloud (S3/Fargate/Cloud Run) is explicitly out of scope.

## Background facts (verified against the code)

- `src/sandbox/worker.rs`: store-mediated helpers `push_job`/`pull_job`/`push_result`/`pull_result`/`cleanup`/`run_worker_turn`, and `SubprocessWorkerProvider { store: Arc<dyn StateStore>, self_exe: PathBuf }` whose `run_turn` = `turn_id=uuid; push_job; Command::new(self_exe).arg("worker").arg("--turn").arg(id).status(); if !success {cleanup; bail}; pull_result; cleanup; result?.ok_or(...)`.
- `src/sandbox/mod.rs` `try_default_provider`: `ProviderKind::Subprocess` arm builds `SubprocessWorkerProvider::new(store, std::env::current_exe()?)`. `SubprocessWorkerProvider` is referenced ONLY here.
- `src/config.rs`: `enum ProviderKind { Local, Subprocess }`; `DeploymentConfig { store: Option<StoreKind>, state_path: Option<String>, provider: Option<ProviderKind> }`.
- `crate::config::paths()` → `Paths { config_file: base/config.toml, skills_dir: base/skills, internal_dir: base/internal, ... }`. The filesystem store default path is `internal_dir/state-store` (used when `state_path` is unset).
- `cmd::worker::run` (the in-container entrypoint) builds `HydratingProvider` directly from `Config::load()` + `default_store` — it does NOT go through `try_default_provider`, so it ignores `provider` and just needs `store` configured.

## 3b-1 assumption (documented)

For the local Docker prover, **`[deployment].state_path` is unset** (so the store resolves to `internal/state-store` on both host and container). `DockerLauncher` mounts the host's `internal/state-store` → the container's `/data/cica/internal/state-store`, where the in-container worker (with `XDG_CONFIG_HOME=/data`) also resolves it. (A custom absolute `state_path` would diverge host↔container; out of scope for the prover.)

## File structure

- Modify `src/sandbox/worker.rs` — add `Launcher` trait, `LaunchedWorkerProvider`, `SubprocessLauncher`, `DockerLauncher`; remove `SubprocessWorkerProvider`.
- Modify `src/sandbox/mod.rs` — `try_default_provider`: Subprocess + Docker arms build `LaunchedWorkerProvider` with the right launcher.
- Modify `src/config.rs` — `ProviderKind::Docker` + `docker_image` field on `DeploymentConfig`.
- Create `Dockerfile` + `.dockerignore` at repo root.

---

### Task 1: `Launcher` trait + `LaunchedWorkerProvider` + `SubprocessLauncher` (refactor, behavior-preserving)

**Files:**
- Modify: `src/sandbox/worker.rs`
- Modify: `src/sandbox/mod.rs`

- [ ] **Step 1: Add the trait, provider, and SubprocessLauncher**

In `src/sandbox/worker.rs`, replace the entire `SubprocessWorkerProvider` block (the `pub struct SubprocessWorkerProvider`, its `impl`, and its `impl SandboxProvider`) with:

```rust
/// Runs the worker for a `turn_id` to completion. `Ok` = clean exit 0;
/// `Err` = launch failure or non-zero exit. Job/result travel via the store.
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, turn_id: &str) -> Result<()>;
}

/// Router-side provider: store-mediated dispatch, delegating the run-to-exit
/// step to a `Launcher` (subprocess, docker, …).
pub struct LaunchedWorkerProvider {
    store: Arc<dyn StateStore>,
    launcher: Box<dyn Launcher>,
}

impl LaunchedWorkerProvider {
    pub fn new(store: Arc<dyn StateStore>, launcher: Box<dyn Launcher>) -> Self {
        Self { store, launcher }
    }
}

#[async_trait]
impl SandboxProvider for LaunchedWorkerProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let turn_id = Uuid::new_v4().to_string();

        push_job(self.store.as_ref(), &turn_id, &job).await?;

        if let Err(e) = self.launcher.launch(&turn_id).await {
            cleanup(self.store.as_ref(), &turn_id).await;
            return Err(e);
        }

        let result = pull_result(self.store.as_ref(), &turn_id).await;
        cleanup(self.store.as_ref(), &turn_id).await;

        result?.ok_or_else(|| anyhow::anyhow!("worker produced no result for turn {turn_id}"))
    }
}

/// Launcher that spawns `cica worker --turn <id>` as a local child process.
pub struct SubprocessLauncher {
    self_exe: PathBuf,
}

impl SubprocessLauncher {
    pub fn new(self_exe: PathBuf) -> Self {
        Self { self_exe }
    }
}

#[async_trait]
impl Launcher for SubprocessLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let status = Command::new(&self.self_exe)
            .arg("worker")
            .arg("--turn")
            .arg(turn_id)
            .status()
            .await
            .context("spawning cica worker")?;
        if !status.success() {
            anyhow::bail!("worker exited with status {status}");
        }
        Ok(())
    }
}
```

(`use` items `Command`, `Uuid`, `PathBuf`, `Arc`, `async_trait`, `Context`, `Result`, `StateStore`, `SandboxProvider`/`TurnJob`/`TurnResult` are already imported. Keep them.)

- [ ] **Step 2: Update the `try_default_provider` Subprocess arm**

In `src/sandbox/mod.rs`, replace the `ProviderKind::Subprocess` arm body:

```rust
        ProviderKind::Subprocess => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = subprocess` requires [deployment].store to be set")
            })?;
            let self_exe = std::env::current_exe()?;
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(worker::SubprocessLauncher::new(self_exe)),
            )))
        }
```

- [ ] **Step 3: Update the launcher contract test**

In `src/sandbox/worker.rs`'s `#[cfg(test)] mod tests`, there is a test that exercises the dispatch contract via `run_worker_turn` + a stub engine (`run_worker_turn_reads_job_and_writes_result`). Keep it. Add a test that drives `LaunchedWorkerProvider` end-to-end with a fake in-process launcher:

```rust
    #[tokio::test]
    async fn launched_provider_dispatches_via_launcher() {
        use crate::config::AiBackend;

        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FilesystemStateStore::new(root.path().to_path_buf()));

        // A fake launcher that, instead of spawning, runs the worker turn
        // in-process against the same store with a stub engine.
        struct FakeLauncher {
            store: std::sync::Arc<FilesystemStateStore>,
        }
        struct StubEngine;
        #[async_trait]
        impl SandboxProvider for StubEngine {
            async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
                Ok(TurnResult {
                    response: "ok".into(),
                    backend_session_id: "sess".into(),
                    cost_usd: None,
                    duration_ms: None,
                })
            }
        }
        #[async_trait]
        impl Launcher for FakeLauncher {
            async fn launch(&self, turn_id: &str) -> Result<()> {
                run_worker_turn(self.store.as_ref(), &StubEngine, turn_id).await
            }
        }

        let provider = LaunchedWorkerProvider::new(
            store.clone(),
            Box::new(FakeLauncher { store: store.clone() }),
        );
        let job = TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: None,
            cwd: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
        };
        let result = provider.run_turn(job).await.unwrap();
        assert_eq!(result.backend_session_id, "sess");
    }
```

(If the test module already has a `StubEngine` or `FakeLauncher`-like helper, reuse it instead of redefining.)

- [ ] **Step 4: Build + test**

Run: `cargo build && cargo test sandbox`
Expected: SUCCESS; the existing 3a worker/provider tests still pass (behavior-preserving), plus the new dispatch test. The `subprocess_provider_*` tests in `mod.rs` still pass (they go through `try_default_provider`).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/worker.rs src/sandbox/mod.rs
git commit -m "refactor(sandbox): generalize worker dispatch behind a Launcher trait"
```
End every commit with: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 2: `DockerLauncher`

**Files:**
- Modify: `src/sandbox/worker.rs`

- [ ] **Step 1: Write the failing test for argv construction**

Add to `src/sandbox/worker.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn docker_launcher_builds_run_args() {
        let l = DockerLauncher::new(
            "cica-worker:latest".into(),
            std::path::PathBuf::from("/host/config.toml"),
            std::path::PathBuf::from("/host/skills"),
            std::path::PathBuf::from("/host/state-store"),
        );
        let args = l.run_args("turn-123");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"/host/config.toml:/data/cica/config.toml:ro".to_string()));
        assert!(args.contains(&"/host/skills:/data/cica/skills:ro".to_string()));
        assert!(args.contains(&"/host/state-store:/data/cica/internal/state-store".to_string()));
        // image then `worker --turn <id>` at the end
        let tail = &args[args.len() - 4..];
        assert_eq!(tail, ["cica-worker:latest", "worker", "--turn", "turn-123"]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sandbox::worker::tests::docker_launcher_builds_run_args`
Expected: FAIL to compile — `DockerLauncher` not found.

- [ ] **Step 3: Implement `DockerLauncher`**

Add to `src/sandbox/worker.rs` (module level):

```rust
/// Launcher that runs `cica worker --turn <id>` inside a one-shot container.
///
/// Mounts the host config, published skills, and filesystem state-store into a
/// `/data/cica`-pinned container (the image sets `XDG_CONFIG_HOME=/data`).
/// `cursor-home`/`claude-home` stay container-local (fresh per turn).
pub struct DockerLauncher {
    image: String,
    config_file: PathBuf,
    skills_dir: PathBuf,
    state_store_dir: PathBuf,
}

impl DockerLauncher {
    pub fn new(image: String, config_file: PathBuf, skills_dir: PathBuf, state_store_dir: PathBuf) -> Self {
        Self { image, config_file, skills_dir, state_store_dir }
    }

    /// The `docker` argv (without the leading `docker`). Pure, for testing.
    fn run_args(&self, turn_id: &str) -> Vec<String> {
        vec![
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{}:/data/cica/config.toml:ro", self.config_file.display()),
            "-v".into(),
            format!("{}:/data/cica/skills:ro", self.skills_dir.display()),
            "-v".into(),
            format!("{}:/data/cica/internal/state-store", self.state_store_dir.display()),
            self.image.clone(),
            "worker".into(),
            "--turn".into(),
            turn_id.into(),
        ]
    }
}

#[async_trait]
impl Launcher for DockerLauncher {
    async fn launch(&self, turn_id: &str) -> Result<()> {
        let status = Command::new("docker")
            .args(self.run_args(turn_id))
            .status()
            .await
            .context("running `docker run` for cica worker")?;
        if !status.success() {
            anyhow::bail!("worker container exited with status {status}");
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test sandbox::worker::tests::docker_launcher_builds_run_args`
Expected: PASS. `cargo build` succeeds (a `dead_code` warning on `DockerLauncher` until Task 4 wires it is acceptable; no `#[allow]`).

- [ ] **Step 5: Commit**

```bash
git add src/sandbox/worker.rs
git commit -m "feat(sandbox): add DockerLauncher (docker run for cica worker)"
```

---

### Task 3: `ProviderKind::Docker` + image config

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/config.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn provider_parses_docker_with_image() {
        let toml = r#"
            [deployment]
            provider = "docker"
            store = "filesystem"
            docker_image = "cica-worker:dev"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Docker));
        assert_eq!(cfg.deployment.docker_image.as_deref(), Some("cica-worker:dev"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::provider_parses_docker_with_image`
Expected: FAIL — `ProviderKind::Docker` / `docker_image` not found.

- [ ] **Step 3: Add the variant and field**

In `src/config.rs`, add `Docker` to `ProviderKind`:

```rust
pub enum ProviderKind {
    Local,
    Subprocess,
    Docker,
}
```

Add to `DeploymentConfig` (alongside `provider`):

```rust
    /// Worker image for `provider = "docker"` (default `cica-worker:latest`).
    #[serde(default)]
    pub docker_image: Option<String>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test config::tests::provider_parses_docker_with_image`
Expected: PASS. `cargo build` — NOTE: adding a `ProviderKind` variant makes the `match` in `try_default_provider` non-exhaustive → it will FAIL to compile until Task 4. That's expected; do Task 4 before the full build, or fold Task 4 in here. Either way the commit below should land with a green build, so **complete Task 4 before committing** (combine the commits) OR add the Docker arm now.

> **Sequencing:** the cleanest is to do Task 3 + Task 4 together (they're both small) and commit once with a green build. The steps are kept separate for clarity.

- [ ] **Step 5: (Commit with Task 4.)**

---

### Task 4: Wire the Docker provider in `try_default_provider`

**Files:**
- Modify: `src/sandbox/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/sandbox/mod.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn docker_provider_requires_a_store() {
        use crate::config::{Config, ProviderKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        assert!(try_default_provider(&cfg).is_err());
    }

    #[test]
    fn docker_provider_built_when_store_present() {
        use crate::config::{Config, ProviderKind, StoreKind};
        let mut cfg = Config::default();
        cfg.deployment.provider = Some(ProviderKind::Docker);
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-docker-test".into());
        assert!(try_default_provider(&cfg).is_ok());
    }
```

- [ ] **Step 2: Add the Docker arm**

In `src/sandbox/mod.rs` `try_default_provider`, add a `ProviderKind::Docker` arm after the Subprocess arm:

```rust
        ProviderKind::Docker => {
            let store = store.ok_or_else(|| {
                anyhow::anyhow!("`provider = docker` requires [deployment].store to be set")
            })?;
            let paths = crate::config::paths()?;
            let image = config
                .deployment
                .docker_image
                .clone()
                .unwrap_or_else(|| "cica-worker:latest".to_string());
            let state_store_dir = match &config.deployment.state_path {
                Some(p) => std::path::PathBuf::from(p),
                None => paths.internal_dir.join("state-store"),
            };
            let launcher = worker::DockerLauncher::new(
                image,
                paths.config_file,
                paths.skills_dir,
                state_store_dir,
            );
            Ok(Box::new(worker::LaunchedWorkerProvider::new(
                store,
                Box::new(launcher),
            )))
        }
```

- [ ] **Step 3: Build + full test**

Run: `cargo build && cargo test`
Expected: SUCCESS; all tests pass (incl. the new docker provider tests + Task 3's config test).

- [ ] **Step 4: Commit (Tasks 3 + 4)**

```bash
git add src/config.rs src/sandbox/mod.rs
git commit -m "feat(sandbox): select DockerLauncher via [deployment] provider = docker"
```

---

### Task 5: Worker `Dockerfile` + `.dockerignore`

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`

- [ ] **Step 1: Create `.dockerignore`**

```
target
.git
docs
.claude
**/*.md
cdk.out
```

- [ ] **Step 2: Create `Dockerfile`**

A multi-stage build: build the release binary, then assemble a slim runtime with `bun`, `cursor-cli`, `claude-code`, and `XDG_CONFIG_HOME=/data`.

```dockerfile
# ---- build stage ----
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin cica

# ---- runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip git \
 && rm -rf /var/lib/apt/lists/*

# Pin cica's data dir to /data/cica (ProjectDirs honors XDG_CONFIG_HOME on Linux),
# so paths.base = /data/cica and the cursor workspace hash = md5("/data/cica").
ENV XDG_CONFIG_HOME=/data
RUN mkdir -p /data/cica/internal/deps

# Bun (both backends' CLIs run on it)
RUN curl -fsSL https://bun.sh/install | bash \
 && cp /root/.bun/bin/bun /usr/local/bin/bun

# Cursor CLI + Claude Code (backend-agnostic image)
RUN curl -fsSL https://cursor.com/install | bash || true
RUN bun install -g @anthropic-ai/claude-code || true

COPY --from=build /src/target/release/cica /usr/local/bin/cica

ENTRYPOINT ["cica"]
```

> The exact installers for `cursor-cli`/`claude-code` may need adjustment to match what `setup::find_cursor_cli`/`find_claude_code` resolve (system `which` vs `internal/deps`). During implementation, verify `cica worker` inside the image finds them; if `find_*` only checks `internal/deps`, install into `/data/cica/internal/deps/...` instead of `/usr/local/bin`. The `|| true` guards let the image build while the exact install path is dialed in; remove them once the install is confirmed. This is the one task that needs iteration against a real `docker build`.

- [ ] **Step 3: Smoke-build the image (manual; needs Docker)**

Run: `docker build -t cica-worker:latest .`
Expected: image builds. Then `docker run --rm cica-worker:latest --help` lists the `worker` subcommand.

> If Docker isn't available in the implementation environment, commit the Dockerfile as-is and mark Step 3 as a manual step in the validation doc (Task 6). The Rust tests do not depend on the image.

- [ ] **Step 4: Commit**

```bash
git add Dockerfile .dockerignore
git commit -m "feat(docker): add worker container image (cica + bun + cursor-cli + claude-code)"
```

---

### Task 6: Lint, fmt, and manual validation doc

**Files:**
- Possibly modify: `src/sandbox/worker.rs`, `src/sandbox/mod.rs` (imports only)
- Modify: `docs/superpowers/plans/2026-06-03-phase3b1-container-worker.md`

- [ ] **Step 1: Clippy gate**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: SUCCESS. `Launcher`/`LaunchedWorkerProvider`/`SubprocessLauncher`/`DockerLauncher` are all reachable now. Remove any unused-import warnings; do NOT blanket-`#[allow(dead_code)]`.

- [ ] **Step 2: Format**

Run: `cargo fmt` then `cargo fmt --check` (expect clean).

- [ ] **Step 3: Full test run**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 4: Append manual validation to this plan**

Append to the END of `docs/superpowers/plans/2026-06-03-phase3b1-container-worker.md`:

```markdown

## Manual validation (local, needs Docker + a configured cica with the Cursor backend)

1. `docker build -t cica-worker:latest .` (verify `cursor-cli`/`claude-code`/`bun` are found inside; iterate the Dockerfile if `cica worker --help` errors on a missing dep).
2. In `config.toml` (state_path unset): `[deployment]\nprovider = "docker"\nstore = "filesystem"`.
3. Send a message. Confirm a `cica-worker` container runs (`docker ps` during the turn), the reply arrives, and `internal/state-store/turns/<id>/` round-trips, with `session/<id>/...` written.
4. **Fresh-container isolation (the headline):** send a follow-up in the same conversation. Because each turn is a brand-new container with an empty `cursor-home`, a correct resume can ONLY come from the store-restored session db. Confirm it remembers. (This is the real isolation proof the same-box subprocess run couldn't give.)
5. Confirm a memory written in step 3 persists.
6. Negative: `provider = "docker"` with no `store` → cica logs the config error and runs in-process (doesn't crash).
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(sandbox): fmt + clippy for docker launcher; document manual validation"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- `Launcher` trait + generalize 3a dispatch (`LaunchedWorkerProvider`, `SubprocessLauncher`) → Task 1.
- `DockerLauncher` (run args + launch) → Task 2.
- Worker contract realized: `XDG_CONFIG_HOME=/data` pin, config + skills + state-store mounts, fresh homes → Tasks 2 (mounts), 5 (image env + deps).
- `provider = "docker"` config + image → Tasks 3, 4.
- Worker `Dockerfile` (cica + bun + both CLIs, no embedding model) → Task 5.
- Skills as a read-only mount → Task 2 (`run_args`) + the contract.
- No new Rust deps; default build unchanged → only existing crates; Docker shelled out.
- Subprocess-parity (refactor behavior-preserving) → Task 1 Step 4.
- Manual fresh-container validation → Task 6.
- Out of scope (Fargate/S3/Cloud Run/registry/secrets/network-result) → not present.

**Placeholder scan:** No "TBD"/"handle errors appropriately". Every code step shows complete code. The Dockerfile `|| true` guards + the iterate-against-`docker build` note (Task 5) are explicit, intentional implementation guidance for the one genuinely environment-dependent artifact — not a placeholder for Rust logic.

**Type consistency:** `Launcher::launch(&self, turn_id: &str) -> Result<()>` identical across Tasks 1–2. `LaunchedWorkerProvider::new(store, Box<dyn Launcher>)` matches Tasks 1, 4. `DockerLauncher::new(image, config_file, skills_dir, state_store_dir)` matches Tasks 2, 4. `run_args(turn_id) -> Vec<String>` consistent between the impl and its test. `ProviderKind::Docker` + `docker_image` consistent across Tasks 3, 4. Mount target paths (`/data/cica/config.toml`, `/data/cica/skills`, `/data/cica/internal/state-store`) match the Dockerfile's `XDG_CONFIG_HOME=/data` pin.

## Next (after this merges)

Phase 3b-2: `FargateLauncher` + feature-gated `S3StateStore` + secrets-to-worker + network result-return (RunTask + DescribeTasks poll) + ECR image publish + the `sprout` CDK + `--all-features` CI. Then 3b-3 (Cloud Run + GCS), and the separate Skills phase (git-backed `ai-skills` + draft persistence + publish-PR).

## Manual validation (local, needs Docker + a configured cica with the Cursor backend)

1. `docker build -t cica-worker:latest .` — verified building on Ubuntu 24.04 (glibc 2.39, required by ort-sys/ONNX). `docker run --rm cica-worker:latest --help` lists the `worker` subcommand.
2. In `config.toml` (leave `state_path` unset): `[deployment]\nprovider = "docker"\nstore = "filesystem"`.
3. Send a message. Confirm a `cica-worker` container runs (`docker ps` during the turn), the reply arrives, and `internal/state-store/turns/<id>/` round-trips, with `session/<id>/...` written.
4. **Fresh-container isolation (the headline):** send a follow-up in the same conversation. Because each turn is a brand-new container with an empty `cursor-home`, a correct resume can ONLY come from the store-restored session db. Confirm it remembers. (This is the real isolation proof the same-box subprocess run couldn't give.)
5. Confirm a memory written in step 3 persists.
6. Negative: `provider = "docker"` with no `store` → cica logs the config error and runs in-process (doesn't crash).

### Image maintenance notes (follow-ups, not blockers)
- The `CURSOR_CLI_VERSION` / `CLAUDE_CODE_VERSION` build ARGs duplicate the version constants in `src/setup.rs` — keep them in sync when those bump (or have the build read them from the source).
- The cursor-agent download is hardcoded to `linux/x64`; parameterize the arch for arm64 hosts/targets (multi-arch).
