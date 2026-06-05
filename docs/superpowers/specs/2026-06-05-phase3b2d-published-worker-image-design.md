# Phase 3b-2d: published worker image + env-driven worker config

**Date:** 2026-06-05
**Status:** Design approved (in conversation), pending spec review
**Parent design:** `docs/superpowers/specs/2026-06-02-distributed-deployment-design.md`
**Predecessors:** 3b-1 (worker `Dockerfile`), 3b-2a/2b (S3 store + Fargate launcher), 3b-2c (`sprout` deployment — built, not yet deployed).
**Repos touched:** `cica` (config + release CI) and `sprout` (task-def + image handling).

## Goal

Make the cica **worker image a first-class published artifact** so deployments consume it instead of building their own, and make the worker **fully configurable from environment variables** so the same generic image runs unchanged on AWS, local Docker, and (later) GCP. Concretely: cica publishes a public worker image to GHCR on each release, and cica's config loader sources the deployment-relevant settings from env. `sprout` then drops its build-from-source step and configures the worker via the task-def env. This unblocks the live deploy (which currently has no published image and would require an emulated source build on an arm64 Mac).

## Why (the gap)

cica ships release **binaries** (`release.yml`) but **no worker image** — the only CI image build is the throwaway `docker-flow` test. So every deployment must compile/build its own image, and `sprout`'s `push-image.sh` builds cica from source (slow under arm64→amd64 emulation; arch tied to the build host). Per "cica is the tech; hosting is configured by others," the worker image should be a published deliverable, and deployment-specific config should arrive as env (the same channel the AI-key secrets already use), not baked into an image.

## Key decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Registry | **GHCR, public** (`ghcr.io/oxiglade/cica-worker`) | Free for public images; natural for the GitHub project; no secrets in the image. |
| Architecture | **`linux/amd64` now**; `arm64` deferred | The Fargate task-def is x86_64, and `cursor-cli`'s arm64-Linux support is unconfirmed. The CI is structured so adding `linux/arm64` is a one-line buildx change once deps are verified. |
| Image contents | Consume the **per-arch release binary** (no in-image `cargo build`); bake bun + cursor-cli + claude-code (as today) | Fast, reproducible builds; the worker runs the exact published binary; same glibc (ubuntu-latest 24.04 ↔ `ubuntu:24.04` base). |
| Image publish | A **release-workflow job** that runs after the binary build, builds the image from the just-built binary artifact, pushes `:<version>` + `:latest` | Self-contained in the release; no dependency on the release being public first. |
| Worker config | **Env-driven**: extend cica's env overlay to `CICA_BACKEND`, `CICA_STORE`, `CICA_S3_BUCKET`, `CICA_S3_REGION` (plus the existing `CICA_*_API_KEY`); `Config::load` falls back to `Config::default()` + overlay when no `config.toml` exists (with a warning) | One immutable generic image; config via the task-def, no per-deployment image, no rebuild on config change. The router (single-box, always has a `config.toml`) is unaffected. |
| sprout image | **Mirror** the published image into ECR (pull GHCR → push ECR:`<version>`); task-def references the ECR image + sets the worker config env | In-region, AWS-native pulls; reuses the existing exec-role ECR perms; `push-image.sh` shrinks to a mirror (no build). (ECR pull-through cache is a cleaner future option.) |

## Part 1 (cica) — env-driven worker config

Extend the env overlay added in 3b-2b. Today `overlay_secrets_from` maps the two AI keys. Generalize it to also map the deployment-relevant non-secret settings, and let `Config::load` start from defaults when the file is absent.

**New env mappings** (applied on top of the loaded-or-default config):

| Env var | Config field | Notes |
| --- | --- | --- |
| `CICA_BACKEND` | `backend` (`AiBackend`) | `"cursor"` / `"claude"` (parse; ignore/warn on unknown) |
| `CICA_STORE` | `deployment.store` (`StoreKind`) | `"s3"` / `"filesystem"` |
| `CICA_S3_BUCKET` | `deployment.s3.bucket` | creates `deployment.s3` if absent |
| `CICA_S3_REGION` | `deployment.s3.region` | creates `deployment.s3` if absent |
| `CICA_CURSOR_API_KEY` | `cursor.api_key` | existing |
| `CICA_CLAUDE_API_KEY` | `claude.api_key` | existing |

`provider` is intentionally **not** env-mapped: the worker runs in-process (`provider` defaults to `Local`), so no env is needed; only the router sets `provider = "fargate"` (in its `config.toml`).

**`Config::load` fallback:** when `paths().config_file` does not exist, log a warning (`no config.toml at <path>; using defaults + environment`) and start from `Config::default()` instead of erroring; then apply the env overlay as today. `Config` and `AiBackend` already derive `Default` (default backend `Claude`), so `Config::default()` is valid. A genuinely-missing config on a normal single-box install now starts with defaults rather than erroring — the warning makes that visible, and the router/EFS always has a file, so this only changes behavior for the intentionally config-less worker.

**Result:** a worker started with `CICA_BACKEND=cursor CICA_STORE=s3 CICA_S3_BUCKET=… CICA_S3_REGION=… CICA_CURSOR_API_KEY=…` and **no `config.toml`** runs correctly.

## Part 2 (cica) — publish the worker image on release

Add a job to `.github/workflows/release.yml` (runs on the same `v*` tag). For `linux/amd64`:
1. Download the `cica-linux-x86_64` binary artifact produced by the existing build job (`actions/download-artifact`).
2. Build the worker image from a Dockerfile path that **consumes the prebuilt binary** (places it at `/usr/local/bin/cica`) instead of the `cargo build` stage, keeping the bun/cursor-cli/claude-code layers and `ENV XDG_CONFIG_HOME=/data`.
3. Log in to GHCR (`docker/login-action` with the `GITHUB_TOKEN`, `packages: write` permission) and push `ghcr.io/oxiglade/cica-worker:<version>` and `:latest`.
4. Ensure the package is **public** (org/repo package visibility set to public — a one-time setting, noted in the release docs).

**Dockerfile:** introduce a way to build from the prebuilt binary (e.g. a build stage selected by a build-arg, or a dedicated `Dockerfile.release` that `COPY`s the binary from the build context). The existing source-build path stays usable for local `docker build`. The exact mechanic is settled in the plan; the contract is "the published image contains the release binary + the three runtimes, no compile."

**Multi-arch note:** the job builds/pushes `linux/amd64` only for now. Adding `linux/arm64` later is a `buildx --platform linux/amd64,linux/arm64` change plus verifying cursor-cli/bun arm64-Linux builds — out of scope here.

## Part 3 (sprout) — consume the published image, configure via env

1. **Image:** replace `scripts/push-image.sh` (build-from-source) with `scripts/mirror-image.sh` — pull `ghcr.io/oxiglade/cica-worker:<version>`, retag to the ECR repo, push `cica-worker:<version>`. No build, correct arch (amd64 manifest). `--platform linux/amd64` is implicit (the published image is amd64).
2. **Task-def env:** add the non-secret worker config as plain `environment` (alongside the existing `CICA_*_API_KEY` `secrets`):
   - `CICA_BACKEND=cursor`
   - `CICA_STORE=s3`
   - `CICA_S3_BUCKET=cica-state-974767452524-eu-central-1`
   - `CICA_S3_REGION=eu-central-1`
   The worker image carries **no `config.toml`**; the env supplies everything.
3. **Remove** the baked-config concept from the worker image path (the old `push-image.sh` layered a `config.toml`).
4. **RUNBOOK:** update step 1/3 — "mirror the published image" instead of "build + push"; note the GHCR image + version.

The CDK `FargateTaskDefinition` container gains the four `environment` entries; everything else (cluster, IAM, networking, the router) is unchanged from 3b-2c.

## Data flow (worker startup, after this phase)

```
ECS RunTask → pull ECR image (mirror of ghcr.io/oxiglade/cica-worker:<version>)
  container: cica worker --turn <id>
    Config::load: no /data/cica/config.toml → defaults + env overlay
      backend=cursor, store=s3, s3.bucket/region from env, cursor.api_key from secret env
    hydrate ← S3, run backend, push result → S3, exit 0
```

## Error handling

- **Unknown `CICA_BACKEND`/`CICA_STORE` value** → log a warning and keep the default/loaded value (don't crash the turn on a typo).
- **No `config.toml` and no env** → defaults (backend `Claude`, no store) + the warning; the turn fails clearly if a required setting (e.g. store) is absent, rather than silently mis-running.
- **GHCR image missing for a version** → the mirror script fails fast (operator pushed no release image for that version); the release docs note that publishing a release publishes the image.
- **Image publish job failure** → the release still publishes binaries; the image job is independent and can be re-run.

## Testing strategy

- **cica unit (Rust, TDD):** extend the existing `overlay_secrets_from`-style tests — a lookup-closure test asserting `CICA_BACKEND`/`CICA_STORE`/`CICA_S3_BUCKET`/`CICA_S3_REGION` map to the right fields (and unknown enum values are ignored with the prior value retained). A test that `Config::load`-style assembly from `Config::default()` + the overlay yields a valid worker config. (Keep the closure form so tests don't touch the global env.)
- **cica image (CI):** the release image job is exercised on a tag; for PR-time confidence, a `docker build` of the release Dockerfile path in CI (no push) confirms it assembles. The existing `docker-flow` fake-backend test continues to validate the image runs a turn.
- **sprout:** `pnpm test` (Template assertions) gains an assertion that the task-def container has the four `CICA_*` config env entries; `mirror-image.sh` gets a `bash -n` check.
- **End-to-end (operator, live):** the first real `RunTask` (3b-2c RUNBOOK) now pulls the published image and runs config-from-env — the real acceptance test.

## Distribution impact

- cica gains a published, public worker image as a release artifact (alongside binaries). Default `cargo build`/`install.sh` unchanged. The env-config overlay adds no dependency (plain config logic), benefiting local/Docker/Fargate uniformly.
- `sprout` no longer compiles cica; it mirrors a prebuilt image and configures via env — smaller, faster, arch-correct.

## Out of scope (later)

- `linux/arm64` image (Graviton / Apple-Silicon native) — pending cursor-cli/bun arm64-Linux verification.
- ECR **pull-through cache** for GHCR (cleaner than the manual mirror) — revisit if the mirror step becomes friction.
- A fully general `CICA_<PATH>` env-config convention for *every* config field — only the worker-relevant keys are mapped now (YAGNI).
- Publishing the image to a second registry (Docker Hub) or to the user's own registry.
- GCP Artifact Registry mirror (3b-3).
