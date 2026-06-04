# Phase 3b-1: Containerized Worker + Local Docker Launcher (no cloud)

**Date:** 2026-06-03
**Status:** Design approved, pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** Phase 3a (`cica worker` + `SubprocessWorkerProvider`, store-mediated dispatch), Phase 2c (Cursor session artifacts — validated on the live box).

## Goal

Run an agent turn inside a one-shot **container** instead of a subprocess, proving the containerized worker round-trip — including **fresh-container filesystem isolation** (an empty `cursor-home`/`claude-home` per turn, reconstructed from the store) — entirely on a local machine with no cloud. Produce the **worker contract** that AWS/GCP hosting (the `sprout` CDK repo) will target, plus a `Launcher` trait shaped so Fargate and Cloud Run drop in later.

## Phase 3b decomposition

- **3b-1 (this spec):** worker `Dockerfile`; a `Launcher` trait + `DockerLauncher` (local `docker run`); generalize 3a's dispatch behind the trait; reuse `FilesystemStateStore` via a mounted volume; pin cwd to `/data/cica`; document the worker contract.
- **3b-2:** `FargateLauncher` + feature-gated `S3StateStore` + secrets-to-worker + network result-return (RunTask + `DescribeTasks` poll) + image publish (ECR) + feature-gated cloud release artifacts + `--all-features` CI. The `sprout` CDK is built here (separate repo).
- **3b-3:** `CloudRunLauncher` (Cloud Run Jobs `jobs.run` + per-execution overrides + poll) + feature-gated `GcsStateStore`.
- **Deferred:** a `KubernetesLauncher` (only if a cluster becomes the target — Docker covers local/self-host, Fargate/Cloud Run cover managed cloud).

## The cica ↔ sprout split (the framing that shapes this)

**cica is the tech; the hosting is configured by others.** cica ships the worker *image* and a *contract*; the deployment IaC lives in a separate repo (`~/Github/sprout`, currently empty — the CDK goes there). The seam between them is the **worker contract** below — the local `DockerLauncher` and sprout's CDK are two implementations of the same contract.

Validated by research: there is no portable cloud-native primitive (ECS is not cloud-agnostic), so the abstraction must be ours — a `Launcher` trait. `RunTask`/Cloud Run Jobs are fire-and-forget, so we keep our cloud-neutral **store-mediated** job+result protocol (from 3a) rather than Step Functions callback tokens; and we pass only the small `turn_id` as a per-execution override, the large `TurnJob` via the store.

## The worker contract (the deliverable sprout targets)

A worker is **any runtime that can:**
1. Run the image with command `cica worker --turn <turn_id>`.
2. Resolve `paths.base` to **`/data/cica`** — achieved by the image setting **`ENV XDG_CONFIG_HOME=/data`** (so `ProjectDirs` → `/data/cica`). This makes the agent's cwd `/data/cica`, so the Cursor workspace hash is `md5("/data/cica") = 5c64d427…` and Claude's slug is stable — **identical on every worker and matching the existing prod sessions.**
3. Provide a valid **`config.toml` at `/data/cica/config.toml`** (backend + creds + the `[deployment].store` config). Locally `DockerLauncher` bind-mounts the host's; in cloud, sprout renders it from secrets.
4. Make the configured **`StateStore` reachable** (filesystem: a volume mounted at the store's `state_path`; S3/GCS: network + creds).
5. Provide **published skills** (read-only) at **`/data/cica/skills`** — the agent reads `SKILL.md`/impl files from disk at runtime. Locally `DockerLauncher` bind-mounts the host `skills/` dir; in cloud the image bakes (or pulls) them from the `ai-skills` repo at a pinned ref. (Same "sourced read-input" pattern as `config.toml`.)
6. Leave `cursor-home`/`claude-home` **container-local and fresh** (the isolation we're proving).
7. Have **network egress** (cursor-cli/claude-code call their APIs).

**Skills are a read-only input in 3b-1.** Worker-*authored* / in-progress (draft) skills are **out of scope here** and are the subject of a dedicated **Skills phase**. Key constraint that phase must solve, surfaced here so it isn't lost: because each turn is a *fresh* worker, a draft skill being iterated across messages **cannot live in worker scratch** — it must persist in **durable state** (leading option: a per-session "draft" area in the `StateStore`, hydrated/dehydrated per turn like the session, with `publish` opening a **PR to `ai-skills`** — source of truth `root-global/ai-skills`, pinned-ref distribution to workers). This is the concretization of the deferred "opt-in durable workspace."

The worker pulls `turns/<id>/job`, runs the turn (hydrating session+memories from the store into the fresh homes), writes `turns/<id>/result`, and exits 0 (non-zero on failure, no result written).

### Cloud (Fargate) worker contract delta (3b-2b)

A Fargate task has no bind mounts, so on top of the base contract:
- **Command override:** the launcher overrides the named container's command to `worker --turn <id>` per turn; the task-def's default command is irrelevant. The task-def must name the worker container per `[deployment.fargate].container_name` (default `cica-worker`).
- **Non-secret config:** sprout supplies `/data/cica/config.toml` (baked into a derived image or written by an entrypoint) with `backend`, `[deployment] store = "s3"`, and `[deployment.s3]`.
- **Secrets:** injected as env from Secrets Manager — `CICA_CURSOR_API_KEY` and/or `CICA_CLAUDE_API_KEY`. cica overlays them onto the loaded config in `Config::load`. Never in the image/S3/file.
- **AWS credentials:** the task IAM role (S3 state-bucket access). The router's role needs `ecs:RunTask`, `ecs:DescribeTasks`, `ecs:StopTask`, and `iam:PassRole` for the task/execution roles.

## Components

### `Launcher` trait — the runtime seam
```rust
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Run the worker for `turn_id` to completion. Ok = clean exit 0; Err = launch
    /// failure or non-zero exit. The job/result are exchanged via the StateStore.
    async fn launch(&self, turn_id: &str) -> Result<()>;
}
```

### Generalize 3a's dispatch behind the trait
3a's `SubprocessWorkerProvider` *is* the store-mediated dispatch (`push_job → spawn → pull_result → cleanup`) with the spawn being the only runtime-specific part. Refactor:
- Extract a single dispatch provider — `LaunchedWorkerProvider { store, launcher: Box<dyn Launcher> }` — implementing `SandboxProvider::run_turn`: `turn_id = uuid; push_job; match launcher.launch(turn_id) { Err → cleanup + bail; Ok → pull_result; cleanup; return }`. (Identical semantics to 3a, including cleanup on both paths.)
- `SubprocessLauncher { self_exe: PathBuf }` — the existing `Command::new(self_exe).arg("worker").arg("--turn").arg(id).status()` logic. So `provider = "subprocess"` becomes `LaunchedWorkerProvider` + `SubprocessLauncher` — **behavior-preserving** for 3a.
- `DockerLauncher` — new (below).

### `DockerLauncher`
```rust
pub struct DockerLauncher {
    image: String,            // e.g. "cica-worker:latest"
    config_file: PathBuf,     // host config.toml to mount (ro)
    skills_dir: PathBuf,      // host published-skills dir to mount (ro)
    state_store_dir: PathBuf, // host filesystem state-store to mount (rw)
}
```
`launch(turn_id)` shells out to `docker run` (via `tokio::process::Command`):
```
docker run --rm \
  -v <config_file>:/data/cica/config.toml:ro \
  -v <skills_dir>:/data/cica/skills:ro \
  -v <state_store_dir>:/data/cica/internal/state-store \
  <image> worker --turn <turn_id>
```
Returns `Ok` on exit 0, `Err` otherwise. The host paths are **derived from `config::paths()`** (the router knows where its own `config.toml`, `skills/`, and state-store live), so the only required config is the image name. `cursor-home`/`claude-home` are *not* mounted → fresh per container.

### Worker `Dockerfile`
A `Dockerfile` (+ `.dockerignore`) at the repo root, built locally for 3b-1 (`docker build -t cica-worker .`), published to a registry in 3b-2.
- Base: a slim Linux image (Debian-slim) with `ca-certificates`.
- `ENV XDG_CONFIG_HOME=/data` (pins `paths.base = /data/cica`).
- Bake in: the release `cica` binary (built for the image's arch), **`bun`**, **`cursor-cli`**, and **`claude-code`** (both backends — the image is backend-agnostic), placed where `setup::find_*` looks (`/data/cica/internal/deps/...`) or on `PATH`. **Not** the `fastembed` model (the worker never re-indexes).
- Entry: `ENTRYPOINT ["cica"]` so `… worker --turn <id>` works.
- Multi-arch note: build `linux/arm64` and/or `linux/amd64` as needed (Apple-Silicon dev vs Fargate/Cloud Run).

### Config
- `crate::config::ProviderKind` gains `Docker`. `[deployment] provider = "docker"` requires a store (like subprocess).
- New `[deployment.docker]` section → `DockerConfig { image: Option<String> }` (default `"cica-worker:latest"`).
- `try_default_provider`: `Docker` → `LaunchedWorkerProvider { store, DockerLauncher{ image, config_file: paths.config_file, state_store_dir: <store path> } }`. (Fargate/CloudRun arms added later return the same provider with a different launcher.)

## Data flow (one turn, `provider = "docker"`)
```
router → LaunchedWorkerProvider.run_turn(job)
  push turns/<id>/job → FilesystemStateStore (host dir)
  DockerLauncher.launch(<id>):
    docker run --rm -v config -v state-store  cica-worker  worker --turn <id>
      [container] XDG_CONFIG_HOME=/data → base=/data/cica
        cica worker: pull job ← store(volume)
          HydratingProvider: restore session+memories ← store INTO FRESH cursor-home
          cursor --resume <id>  (cwd=/data/cica → hash 5c64…)  ← real isolation test
          capture session + push, push memories → store(volume)
        push turns/<id>/result → store ; exit 0
    await container exit 0
  pull turns/<id>/result ← store → TurnResult
  cleanup turns/<id>
→ router posts response
```

## Error handling
- **Launch failure / non-zero container exit / missing result** → `run_turn` returns `Err` (surfaced as the turn error); turn blobs cleaned up. No in-process fallback (a *config* error still falls back per Phase 3a's infallible `default_provider`).
- **`docker` not installed / daemon down** → `docker run` errors → turn error with a clear message; config-validation can also check `docker` availability at startup (best-effort warn).
- **Image missing** → `docker run` fails fast; the contract doc tells the operator to build/pull it.

## Testing strategy
- **Subprocess parity (refactor):** after extracting `LaunchedWorkerProvider` + `SubprocessLauncher`, the existing 3a worker tests (`run_worker_turn_*`, provider dispatch) pass unchanged — proving the refactor is behavior-preserving.
- **`Launcher` contract test:** a fake `Launcher` that synchronously runs `run_worker_turn` against a `FilesystemStateStore` with a stub engine → asserts `LaunchedWorkerProvider` pushes the job, the (fake) launch produces a result, and it's returned + cleaned up. (Same shape as 3a's contract test.)
- **`DockerLauncher` command construction (unit):** assert the built `docker run` argv contains the image, the two `-v` mounts (config ro + state-store), and `worker --turn <id>` — without invoking Docker.
- **Local Docker integration (manual / CI-gated):** `docker build` the image, set `provider = "docker"`, run a real turn end-to-end; assert the reply arrives and `turns/<id>/` round-trips. Gated behind Docker availability (not in unit CI).
- **Fresh-container isolation (the headline manual validation):** with `provider = "docker"` + Cursor, send a message (captures `session/<id>` to the store), send a **follow-up** — and confirm it resumes **even though each container starts with an empty `cursor-home`**. This is the real proof the same-box tests couldn't give: the only way the follow-up can remember is restore-from-store into a fresh container.

## Distribution impact
- The **default `cargo build` and `install.sh` are unchanged.** The `Launcher`/`DockerLauncher`/`LaunchedWorkerProvider` are always compiled but add **no new Rust dependencies** (DockerLauncher shells out to `docker`). The container path is opt-in via config.
- New artifacts: a `Dockerfile` + `.dockerignore`. Building/publishing the image is a separate step (local `docker build` for 3b-1; ECR/registry publish in 3b-2). The image build can be added to CI now or in 3b-2.
- S3/GCS stores and their SDKs remain **feature-gated**, introduced in 3b-2/3b-3 — not here.

## Out of scope (3b-2+)
`FargateLauncher`, `CloudRunLauncher`, `S3StateStore`/`GcsStateStore` + cloud SDKs, secrets injection, network result-return + task-status polling, image registry publish, the `sprout` CDK, multi-arch release pipeline, k8s.

## Open questions / future work
- Whether to bake deps via the deps_dir or system `PATH` in the image — settle during implementation by what `setup::find_*` resolves cleanly in-container.
- Cold-start cost of `docker run` per turn locally — acceptable for the prover; cloud cold-start is a 3b-2 concern.
- Whether the router should reuse a warm worker (Approach B) — still deferred.
