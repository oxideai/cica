# Phase 3b-2d: published worker image + env-driven config Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cica publish a generic public worker image to GHCR on release and make the worker fully configurable from environment variables, then simplify `sprout` to reference that image with config via the task-def env.

**Architecture:** cica's config loader gains env mappings for the deployment settings (+ a no-`config.toml` fallback); `release.yml` builds **lean + cloud** binary variants (via a `cloud` umbrella feature) and a new job publishes `ghcr.io/oxiglade/cica-worker:<version>` (amd64, built from the prebuilt cloud binary via a `BIN_SOURCE` Dockerfile arg); `install.sh` gains a `CICA_VARIANT=cloud` selector. `sprout` then drops its ECR repo + build script and points the task-def at the public GHCR image with the four `CICA_*` config env vars.

**Tech Stack:** Rust 2024 (`cica`), GitHub Actions + Docker buildx, CDK TypeScript (`sprout`, aws-cdk-lib 2.189.1). Two repos: `cica` (Tasks 1–4) then `sprout` (Tasks 5–6).

---

## Background facts (verified)

- `src/config.rs`:
  - `Config::load()` (line ~416): reads `paths().config_file`, errors if missing, `toml::from_str`, `apply_env_overlay`, `Ok`.
  - `apply_env_overlay` → `overlay_secrets_from(get)` (line ~437) maps `CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY`. Two tests call `overlay_secrets_from` (lines ~601, ~610).
  - `Config` + `AiBackend` derive `Default` (default backend `Claude`). `AiBackend` serde `lowercase` (`claude`/`cursor`). `StoreKind` serde `lowercase` (`filesystem`/`s3`). `S3Config { bucket: String, region: Option<String>, prefix, endpoint }`. `DeploymentConfig.s3: Option<S3Config>`, `.store: Option<StoreKind>`.
- `Cargo.toml [features]`: `default = []`, `s3 = ["dep:aws-config", "dep:aws-sdk-s3"]`, `fargate = ["s3", "dep:aws-sdk-ecs"]`.
- `Dockerfile`: `build` stage compiles `cargo build --release --bin cica` (no features); runtime stage installs bun + cursor-cli (`linux/x64`, amd64-only) + claude-code, `ENV XDG_CONFIG_HOME=/data`, `COPY --from=build … cica`, `ENTRYPOINT ["cica"]`.
- `.github/workflows/release.yml`: `on: push: tags: v*`; `build` matrix → `cica-linux-x86_64`/`-aarch64`/`cica-macos-aarch64` via `cargo build --release --target …` (no features); `upload` job publishes the release.
- `install.sh`: `CICA_VERSION` (no-`v`; prepends `v`) or `latest`; downloads `cica-$OS-$ARCH` from `oxiglade/cica` releases; installs to `/usr/local/bin` or `~/.local/bin`.
- `sprout` (`~/Github/sprout`): `SproutFleetStack` has `workerRepo` (ECR) + a task-def container `image: fromEcrRepository(this.workerRepo, cicaVersion(this))`; `scripts/push-image.sh` builds from source; `router-stack.ts` user-data installs via `install.sh … CICA_VERSION=${cicaVersion}`; `scripts/update-router.sh`.

---

### Task 1 (cica): env-driven worker config + no-config fallback

**Files:** Modify `src/config.rs`.

- [ ] **Step 1: Write the failing tests**

In `src/config.rs`'s `#[cfg(test)] mod tests`, **update the two existing calls** `cfg.overlay_secrets_from(...)` → `cfg.overlay_from_env(...)` (the fn is renamed below), and **add**:
```rust
    #[test]
    fn env_overlay_sets_backend_store_and_s3() {
        let mut cfg = Config::default();
        let env = |k: &str| match k {
            "CICA_BACKEND" => Some("cursor".to_string()),
            "CICA_STORE" => Some("s3".to_string()),
            "CICA_S3_BUCKET" => Some("cica-state".to_string()),
            "CICA_S3_REGION" => Some("eu-central-1".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.backend, AiBackend::Cursor);
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
        let s3 = cfg.deployment.s3.unwrap();
        assert_eq!(s3.bucket, "cica-state");
        assert_eq!(s3.region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn env_overlay_ignores_unknown_backend() {
        let mut cfg = Config::default();
        let before = cfg.backend;
        cfg.overlay_from_env(|k| (k == "CICA_BACKEND").then(|| "bogus".to_string()));
        assert_eq!(cfg.backend, before); // unchanged, not crashed
    }

    #[test]
    fn worker_config_assembles_from_defaults_plus_env() {
        // Mirrors the no-config.toml worker: Config::default() + env overlay.
        let mut cfg = Config::default();
        let env = |k: &str| match k {
            "CICA_BACKEND" => Some("cursor".to_string()),
            "CICA_STORE" => Some("s3".to_string()),
            "CICA_S3_BUCKET" => Some("b".to_string()),
            "CICA_S3_REGION" => Some("r".to_string()),
            "CICA_CURSOR_API_KEY" => Some("sekret".to_string()),
            _ => None,
        };
        cfg.overlay_from_env(env);
        assert_eq!(cfg.backend, AiBackend::Cursor);
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("sekret"));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test config::tests::env_overlay_sets_backend_store_and_s3 config::tests::worker_config_assembles_from_defaults_plus_env config::tests::env_overlay_ignores_unknown_backend`
Expected: FAIL — `overlay_from_env` not found.

- [ ] **Step 3: Rename + extend the overlay**

Replace `overlay_secrets_from` and its doc with the generalized `overlay_from_env` (rename), and update `apply_env_overlay` to call it:
```rust
    /// Overlay deployment-relevant config and credential secrets from the
    /// process environment. Lets a cloud worker run with NO `config.toml` —
    /// everything (backend, store, S3 coords, AI keys) comes from the task env.
    pub(crate) fn apply_env_overlay(&mut self) {
        self.overlay_from_env(|k| std::env::var(k).ok());
    }

    /// Env overlay core, parameterized by a lookup so it is testable without
    /// touching the global process environment.
    fn overlay_from_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("CICA_CURSOR_API_KEY") {
            self.cursor.api_key = Some(v);
        }
        if let Some(v) = get("CICA_CLAUDE_API_KEY") {
            self.claude.api_key = Some(v);
        }
        if let Some(v) = get("CICA_BACKEND") {
            match v.as_str() {
                "cursor" => self.backend = AiBackend::Cursor,
                "claude" => self.backend = AiBackend::Claude,
                other => tracing::warn!("ignoring unknown CICA_BACKEND={other}"),
            }
        }
        if let Some(v) = get("CICA_STORE") {
            match v.as_str() {
                "s3" => self.deployment.store = Some(StoreKind::S3),
                "filesystem" => self.deployment.store = Some(StoreKind::Filesystem),
                other => tracing::warn!("ignoring unknown CICA_STORE={other}"),
            }
        }
        if let Some(v) = get("CICA_S3_BUCKET") {
            self.deployment.s3.get_or_insert_with(Default::default).bucket = v;
        }
        if let Some(v) = get("CICA_S3_REGION") {
            self.deployment.s3.get_or_insert_with(Default::default).region = Some(v);
        }
    }
```

- [ ] **Step 4: Add the no-config fallback in `load`**

Replace the body of `Config::load`:
```rust
    pub fn load() -> Result<Self> {
        let path = paths()?.config_file;
        let mut config: Config = match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content)
                .with_context(|| format!("Could not parse config file: {path:?}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("no config.toml at {path:?}; using defaults + environment");
                Config::default()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Could not read config file: {path:?}"));
            }
        };
        config.apply_env_overlay();
        Ok(config)
    }
```

- [ ] **Step 5: Run to verify pass + gates**

Run: `cargo test config::tests` → all pass (the renamed + new tests).
Run: `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt` then `cargo fmt --check` → clean. (`tracing` is already a dependency used elsewhere; confirm the `tracing::warn!` import path matches the file's usage — the codebase uses `tracing::` qualified elsewhere.)

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
feat(config): env-drive worker config (backend/store/s3) + no-config fallback

Extends the env overlay so a worker runs with no config.toml — backend,
store, and S3 coords come from CICA_* env (same channel as the AI keys).
Config::load now starts from defaults (with a warning) when the file is
absent, so the generic worker image needs no baked config.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2 (cica): `cloud` umbrella feature

**Files:** Modify `Cargo.toml`.

- [ ] **Step 1: Add the feature**

In `[features]`, add `cloud` (the single knob the release builds against; grows to add providers like `cloudrun` later without changing the release pipeline):
```toml
[features]
default = []
s3 = ["dep:aws-config", "dep:aws-sdk-s3"]
fargate = ["s3", "dep:aws-sdk-ecs"]
cloud = ["fargate"]
```

- [ ] **Step 2: Verify**

Run: `cargo build --features cloud` → succeeds (equivalent to `--features fargate` today; pulls aws SDKs).
Run: `cargo build` (default) → still lean (no AWS crates).
Run: `cargo clippy --features cloud --all-targets -- -D warnings` → clean.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "$(cat <<'EOF'
feat(build): add cloud umbrella feature (= fargate today)

One knob for the cloud release variant; grows to include future providers
(cloudrun) without changing the release/image pipeline or artifact count.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3 (cica): Dockerfile selectable binary source

**Files:** Modify `Dockerfile`.

> Restructure so the image binary comes from either a source compile (default; local `docker build`) or a prebuilt binary placed in the build context (release CI — no compile, consumes the already-built cloud binary). The runtime stage (bun/cursor/claude) is unchanged.

- [ ] **Step 1: Rewrite `Dockerfile`**

Replace the whole `Dockerfile` with:
```dockerfile
# syntax=docker/dockerfile:1
# Binary source: "compile" (default; builds from source — local dev) or
# "prebuilt" (release CI provides ./cica-bin in the context — no compile).
ARG BIN_SOURCE=compile

# ---- compile stage (source builds) ----
# Ubuntu 24.04 (glibc 2.39) — required by the pre-built ONNX Runtime native lib.
FROM ubuntu:24.04 AS compile
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config libssl-dev cmake \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
ARG CICA_FEATURES=
WORKDIR /src
COPY . .
RUN cargo build --release ${CICA_FEATURES:+--features ${CICA_FEATURES}} --bin cica \
 && cp target/release/cica /cica

# ---- prebuilt stage (release CI) ----
FROM ubuntu:24.04 AS prebuilt
COPY cica-bin /cica
RUN chmod +x /cica

# ---- pick the binary ----
FROM ${BIN_SOURCE} AS binsrc

# ---- runtime stage ----
# Match the compile-stage glibc (Ubuntu 24.04 = glibc 2.39).
FROM ubuntu:24.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip git \
 && rm -rf /var/lib/apt/lists/*

# Pin cica's data dir to /data/cica (ProjectDirs honors XDG_CONFIG_HOME on Linux).
ENV XDG_CONFIG_HOME=/data

RUN mkdir -p \
      /data/cica/internal/deps/bun \
      /data/cica/internal/deps/cursor-cli \
      /data/cica/internal/deps/claude-code \
      /data/cica/internal/claude-home \
      /data/cica/internal/cursor-home

# -- Bun --
RUN curl -fsSL https://bun.sh/install | BUN_INSTALL=/usr/local bash \
 && bun --version

# -- Cursor CLI (cursor-agent) -- (amd64 / linux x64 only)
ARG CURSOR_CLI_VERSION=2026.01.28-fd13201
RUN curl -fsSL \
      "https://downloads.cursor.com/lab/${CURSOR_CLI_VERSION}/linux/x64/agent-cli-package.tar.gz" \
      -o /tmp/cursor-agent.tar.gz \
 && tar -xzf /tmp/cursor-agent.tar.gz --strip-components=1 \
      -C /data/cica/internal/deps/cursor-cli \
 && chmod +x /data/cica/internal/deps/cursor-cli/cursor-agent \
 && rm /tmp/cursor-agent.tar.gz

# -- Claude Code --
ARG CLAUDE_CODE_VERSION=2.1.32
RUN cd /data/cica/internal/deps/claude-code \
 && bun add "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"

COPY --from=binsrc /cica /usr/local/bin/cica

ENTRYPOINT ["cica"]
```

- [ ] **Step 2: Verify the compile path still works**

The existing CI `docker-flow` job builds `docker build -t cica-worker:latest .` (defaults `BIN_SOURCE=compile`, lean) and runs the fake-backend test — that must still pass. If Docker is available locally, confirm the default build succeeds:
`docker build -t cica-worker:local .` → builds from source (this is slow; OK to skip locally and rely on the `docker-flow` CI job, which exercises exactly this default path).
Verify the prebuilt path parses: `BIN_SOURCE` + `FROM ${BIN_SOURCE} AS binsrc` is BuildKit syntax (GitHub Actions uses BuildKit). The release job (Task 4) exercises it for real.

> Note: `FROM ${BIN_SOURCE} AS binsrc` requires BuildKit (default in modern Docker + GH Actions `docker/build-push-action`). If a very old local Docker without BuildKit is used, set `DOCKER_BUILDKIT=1`.

- [ ] **Step 3: Commit**

```bash
git add Dockerfile
git commit -m "$(cat <<'EOF'
refactor(docker): selectable binary source (compile | prebuilt)

Release CI builds the image from the prebuilt cloud binary (BIN_SOURCE=
prebuilt, ./cica-bin in context) — no in-image compile; local docker build
still compiles from source by default. Runtime stage unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4 (cica): release variants + image publish job + install.sh selector

**Files:** Modify `.github/workflows/release.yml`, `install.sh`.

- [ ] **Step 1: Build lean + cloud variants in the matrix**

In `release.yml`, replace the `build` job's `matrix.include` and the `Build` step. New matrix (adds a `features` column; Linux gets a lean + a cloud row):
```yaml
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            name: cica-linux-x86_64
            features: ""
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            name: cica-linux-x86_64-cloud
            features: "--features cloud"
          - os: ubuntu-24.04-arm
            target: aarch64-unknown-linux-gnu
            name: cica-linux-aarch64
            features: ""
          - os: ubuntu-24.04-arm
            target: aarch64-unknown-linux-gnu
            name: cica-linux-aarch64-cloud
            features: "--features cloud"
          - os: macos-latest
            target: aarch64-apple-darwin
            name: cica-macos-aarch64
            features: ""
```
Change the `Build` step:
```yaml
      - name: Build
        run: cargo build --release --target ${{ matrix.target }} ${{ matrix.features }}
```
(The `upload` job already globs `release/*`, so the new `-cloud` assets publish without changes.)

> Verify in CI: the `--features cloud` build pulls `aws-lc-rs` (via the AWS SDKs). If the cloud build fails on a Linux runner for a missing C build tool (cmake/nasm/perl for `aws-lc-sys`), add an apt-install step before the build for that matrix row. The 3b-2a `s3-store` CI job already compiled `--features s3` on `ubuntu-latest` cleanly, so this is expected to work as-is; flag it if it doesn't.

- [ ] **Step 2: Add the image-publish job**

Append a new job to `release.yml`:
```yaml
  image:
    runs-on: ubuntu-latest
    needs: build
    if: startsWith(github.ref, 'refs/tags/')
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Download cloud binary (amd64)
        uses: actions/download-artifact@v4
        with:
          name: cica-linux-x86_64-cloud
          path: .

      - name: Stage binary for the image context
        run: |
          mv cica-linux-x86_64-cloud cica-bin
          chmod +x cica-bin

      - name: Derive image tag (strip leading v)
        id: ver
        run: echo "version=${GITHUB_REF_NAME#v}" >> "$GITHUB_OUTPUT"

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build + push worker image (amd64)
        uses: docker/build-push-action@v6
        with:
          context: .
          build-args: |
            BIN_SOURCE=prebuilt
          push: true
          tags: |
            ghcr.io/oxiglade/cica-worker:${{ steps.ver.outputs.version }}
            ghcr.io/oxiglade/cica-worker:latest
```
The image tag is the no-`v` version (`0.8.0`), matching `install.sh`/sprout's `cicaVersion` convention. The `cica-bin` (cloud binary) sits in the context root; the Dockerfile's `prebuilt` stage `COPY cica-bin /cica`.

- [ ] **Step 3: Add a `--cloud` flag to `install.sh`**

`install.sh` should select the variant via a **flag**, not an env var. Add argument parsing near the top (after `CICA_VERSION` is set, before the download), defaulting to the lean variant:
```sh
# Variant selection: pass --cloud to install the cloud build (AWS/GCP features);
# default is the lean single-box build.
VARIANT_SUFFIX=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --cloud) VARIANT_SUFFIX="-cloud" ;;
        --lean) VARIANT_SUFFIX="" ;;
        *) echo "Unknown option: $1 (use --cloud or --lean)" >&2; exit 1 ;;
    esac
    shift
done
```
And change the two `DOWNLOAD_URL` assignments to append `$VARIANT_SUFFIX` to the asset name:
```sh
        DOWNLOAD_URL="$CICA_BASE_URL/latest/download/cica-$OS-$ARCH$VARIANT_SUFFIX"
        # and:
        DOWNLOAD_URL="$CICA_BASE_URL/download/v$CICA_VERSION/cica-$OS-$ARCH$VARIANT_SUFFIX"
```
Invoked via a pipe as `curl … | sh -s -- --cloud` (the `-s --` forwards the flag to the script). Default (no flag) = lean, so single-box `curl | sh` users are unaffected.

> Check `install.sh` doesn't already consume positional args for another purpose; if it does, integrate `--cloud` into the existing parser rather than adding a second loop.

- [ ] **Step 4: Verify**

Run: `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); print('image' in d['jobs'] and len(d['jobs']['build']['strategy']['matrix']['include'])==5)"` → `True`.
Run: `bash -n install.sh` → no output (syntax OK).
Run: `sh -n install.sh` → OK (the script is `sh`-targeted).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml install.sh
git commit -m "$(cat <<'EOF'
ci: publish ghcr.io/oxiglade/cica-worker on release + lean/cloud variants

release.yml builds lean + cloud binaries per Linux arch and a new job builds
the amd64 worker image from the prebuilt cloud binary and pushes it to GHCR.
install.sh gains a --cloud flag to pull the cloud binary (default lean).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

> **One-time manual step** (document in the PR / release notes, not automatable here): after the first image push, set the GHCR package `cica-worker` visibility to **public** (GitHub → org Packages → cica-worker → Package settings → Change visibility). Until then, Fargate can't pull it anonymously.

---

### Task 5 (sprout): reference the GHCR image + config env, drop ECR

**Files:** Modify `lib/fleet-stack.ts`, `test/fleet-stack.test.ts`. Work in `/Users/dcvz/Github/sprout`.

- [ ] **Step 1: Update the tests**

In `test/fleet-stack.test.ts`: **remove** the ECR assertion (the test `creates the worker AI-keys secret and the ECR repo` — split it so it only checks the secret), and **add** assertions for the GHCR image + config env. Replace that test with:
```ts
test("creates the worker AI-keys secret", () => {
  const t = synth();
  t.hasResourceProperties("AWS::SecretsManager::Secret", { Name: "cica/worker/ai-keys" });
});

test("no ECR repository is created", () => {
  const t = synth();
  t.resourceCountIs("AWS::ECR::Repository", 0);
});

test("task-def uses the public GHCR image and sets the worker config env", () => {
  const t = synth();
  t.hasResourceProperties("AWS::ECS::TaskDefinition", {
    ContainerDefinitions: Match.arrayWith([
      Match.objectLike({
        Name: "cica-worker",
        Image: Match.stringLikeRegexp("ghcr\\.io/oxiglade/cica-worker:"),
        Environment: Match.arrayWith([
          { Name: "CICA_BACKEND", Value: "cursor" },
          { Name: "CICA_STORE", Value: "s3" },
          { Name: "CICA_S3_BUCKET", Value: "cica-state-974767452524-eu-central-1" },
          { Name: "CICA_S3_REGION", Value: "eu-central-1" },
        ]),
      }),
    ]),
  });
});
```

- [ ] **Step 2: Run → fail**

Run: `pnpm test` → the new image/env/no-ECR tests fail.

- [ ] **Step 3: Implement**

In `lib/fleet-stack.ts`:
- **Remove** the ECR import + the `workerRepo` field + its construction (`new ecr.Repository(...)`) and any `WorkerRepo` output. Remove `import * as ecr from "aws-cdk-lib/aws-ecr";`.
- Change the container `image` from `ecs.ContainerImage.fromEcrRepository(this.workerRepo, cicaVersion(this))` to:
```ts
      image: ecs.ContainerImage.fromRegistry(
        `ghcr.io/oxiglade/cica-worker:${cicaVersion(this)}`,
      ),
```
- Add the non-secret config `environment` to `addContainer` (alongside the existing `secrets`):
```ts
      environment: {
        CICA_BACKEND: "cursor",
        CICA_STORE: "s3",
        CICA_S3_BUCKET: "cica-state-974767452524-eu-central-1",
        CICA_S3_REGION: "eu-central-1",
      },
```

- [ ] **Step 4: Verify**

Run: `pnpm test` → all pass.
Run: `pnpm cdk synth -c efsFileSystemId=fs-0000000000000000000` → both stacks synth; confirm no `AWS::ECR::Repository` and the container `Image` is the GHCR ref.
> Note: removing `workerRepo` also removes the exec role's auto-added ECR pull statements; the exec role keeps `AmazonECSTaskExecutionRolePolicy` (Logs). That's correct — a public GHCR pull needs no registry auth.

- [ ] **Step 5: Commit**

```bash
cd /Users/dcvz/Github/sprout
git add lib/fleet-stack.ts test/fleet-stack.test.ts
git commit -m "$(cat <<'EOF'
feat(fleet): use public GHCR worker image + env config; drop ECR

Task-def references ghcr.io/oxiglade/cica-worker:<version> directly and sets
the worker config via env (CICA_BACKEND/STORE/S3_*). The ECR repo + build
step are gone.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6 (sprout): router uses the cloud variant, drop the build script, update RUNBOOK

**Files:** Modify `lib/router-stack.ts`, `scripts/update-router.sh`, `RUNBOOK.md`, `README.md`; delete `scripts/push-image.sh`. Work in `/Users/dcvz/Github/sprout`.

- [ ] **Step 1: Router installs the cloud variant**

In `lib/router-stack.ts`'s user-data, the cica install line must request the **cloud** binary (the router dispatches via Fargate → needs `--features fargate`). Change the install command to pass the `--cloud` flag:
```ts
      `sudo -u ubuntu bash -c 'curl -fsSL https://raw.githubusercontent.com/oxiglade/cica/main/install.sh | CICA_VERSION=${cicaVersion} sh -s -- --cloud'`,
```
Run `pnpm cdk synth -c efsFileSystemId=fs-0000000000000000000` → confirm the user-data now contains `sh -s -- --cloud`. (`pnpm test` unaffected — no assertion on this string; optionally add one.)

- [ ] **Step 2: `update-router.sh` uses the cloud variant**

In `scripts/update-router.sh`, change the SSM command's install line to pass the `--cloud` flag:
```bash
\"sudo -u ubuntu bash -c 'curl -fsSL https://raw.githubusercontent.com/oxiglade/cica/main/install.sh | CICA_VERSION=${CICA_VERSION} sh -s -- --cloud'\",\
```
Run `bash -n scripts/update-router.sh`.

- [ ] **Step 3: Delete the build script**

```bash
git rm scripts/push-image.sh
```
(The worker image is now published by cica's release; sprout no longer builds it.)

- [ ] **Step 4: Update RUNBOOK + README**

In `RUNBOOK.md`:
- **Step 1:** remove the `./scripts/push-image.sh` line. Replace with a note: "The worker image is `ghcr.io/oxiglade/cica-worker:$CICA_VERSION`, published by cica's release — nothing to build. (First time only: ensure the GHCR package is public.)"
- **Prereqs:** Docker is no longer required for sprout (only AWS creds + pnpm). Update the prereq line.
- Confirm the worker `config.toml` no longer needs baking anywhere (the task-def env covers it) — remove any mention.

In `README.md`: drop references to building/pushing the image; note the worker image is cica's published GHCR artifact.

- [ ] **Step 5: Verify**

Run: `pnpm test` → 12 still pass (the fleet tests updated in Task 5).
Run: `pnpm cdk synth -c efsFileSystemId=fs-0000000000000000000` → both stacks synth.
Run: `bash -n scripts/update-router.sh` → OK. Confirm `scripts/push-image.sh` is gone (`git status`).
Run: `python3 -c "import yaml" 2>/dev/null; grep -c push-image RUNBOOK.md || true` → 0 (no stale references).

- [ ] **Step 6: Commit**

```bash
cd /Users/dcvz/Github/sprout
git add -A
git commit -m "$(cat <<'EOF'
feat(router): pull cloud install variant; drop image build; update RUNBOOK

Router/update scripts install cica with the --cloud flag. push-image.sh is
removed — the worker image is cica's published GHCR artifact. RUNBOOK no
longer builds an image (Docker not needed for sprout).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- Env-driven worker config (`CICA_BACKEND`/`CICA_STORE`/`CICA_S3_BUCKET`/`CICA_S3_REGION`) + no-`config.toml` fallback → Task 1.
- `cloud` umbrella feature (`cloud = ["fargate"]`, grows later) → Task 2.
- Dockerfile consumes the prebuilt cloud binary (no compile in the release path) → Task 3.
- `release.yml` lean + cloud variants + the GHCR image-publish job; `install.sh` `CICA_VARIANT` selector → Task 4.
- sprout: reference GHCR directly, drop ECR + build script, config via task-def env → Tasks 5, 6.
- Router uses the cloud variant (it dispatches via Fargate) → Task 6.
- amd64-only (cursor-cli is `linux/x64`) → Tasks 3, 4 (single amd64 image; arm64 deferred per spec).
- GHCR public one-time setting → noted in Task 4.

**Placeholder scan:** No "TBD"/"handle appropriately". The CI-specific verify notes (aws-lc-rs build deps on the Linux runner; BuildKit for `FROM ${ARG}`) are explicit "expected to work; flag if not" guidance for genuinely environment-dependent CI behavior — the honest pattern used in prior phases — not placeholders for logic. The GHCR-public step is a real one-time manual action (a GitHub UI setting), flagged as such.

**Type/name consistency:** `overlay_from_env` is renamed consistently (definition + `apply_env_overlay` caller + the two updated existing tests + the three new tests). Env var names match across cica (Task 1), the task-def (Task 5), and the spec: `CICA_BACKEND`/`CICA_STORE`/`CICA_S3_BUCKET`/`CICA_S3_REGION` + `CICA_CURSOR_API_KEY`/`CICA_CLAUDE_API_KEY`. The image ref `ghcr.io/oxiglade/cica-worker:<version>` is identical in the release job (Task 4, no-`v` tag), the install/router variant, and the sprout `fromRegistry` (Task 5). `BIN_SOURCE`/`CICA_FEATURES`/`cica-bin` are consistent between the Dockerfile (Task 3) and the release image job (Task 4). the `--cloud` flag is consistent between `install.sh` (Task 4), the router user-data, and `update-router.sh` (Task 6). The image tag (`${GITHUB_REF_NAME#v}` → `0.8.0`) matches sprout's `cicaVersion` (no-`v`).

## Next (after this merges)

- Cut the cica `0.8.0` release (`Cargo.toml` bump → tag `v0.8.0` → release.yml publishes binaries + the GHCR image; set the package public once).
- Push `sprout`; walk `RUNBOOK.md` for the first real `RunTask` (now pulling the published image, config-from-env) — the live acceptance test.
- Deferred: `linux/arm64` image (verify cursor-cli/bun arm64-Linux); ECR pull-through cache + image slimming as part of the cold-start optimization; GCP (`cloud = ["fargate", "cloudrun"]`, `GcsStateStore`).
