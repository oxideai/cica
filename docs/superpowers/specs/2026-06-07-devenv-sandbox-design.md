# devenv-Capable Worker Sandbox Design (River strategy — Phase C1)

**Goal:** Give the ephemeral Fargate worker the ability to build and test a cloned repo using that repo's own `devenv` (Nix) environment — so the ticket→PR flow (C3) can verify its work — by installing Nix + devenv in the worker image and building the shell cold at runtime.

**Architecture:** The worker image gains a pinned Nix + devenv installation configured to pull from the public Nix binary caches. At runtime, in a cloned repo that ships a `devenv.{nix,yaml,lock}`, the agent runs `devenv shell -- <cmd>` to get the repo's real toolchain; the closure downloads from the caches into the worker's local `/nix/store` (cold, once per worker). No private cache, EFS warm store, or baked repo closures yet — those are deliberately deferred.

**Tech stack:** Docker (cica `Dockerfile`, Ubuntu 24.04 runtime stage), Nix, devenv, Fargate.

---

## 1. Context & motivation

The worker image today (cica `Dockerfile`, runtime stage on `ubuntu:24.04`) ships only `ca-certificates`, `curl`, `unzip`, `git`, Bun, and Claude Code, and runs as a non-root user (`cica`, uid 10001 — claude-code refuses `--dangerously-skip-permissions` as root). The agent can clone a repo but cannot install its dependencies, build it, or run its tests, so it cannot open a *verified* PR. This is the blocker for Phase C.

Two of Root's repos already ship `devenv` configs, and devenv is the standard way for a machine to reproduce a repo's dev environment (same env the human devs use; mirrors River's use of Nix). So devenv is the dependency-provisioning backbone for the worker sandbox.

### Decisions locked in brainstorming
1. **Build the shell cold at runtime.** The worker runs `devenv shell` per turn, pulling from public binary caches. Simplicity now; optimize later. We explicitly accept slow first-use.
2. **No baking of repo closures into the worker image.** Baking would couple the worker-image release cycle to every repo's `devenv.lock` and balloon the image — unworkable across several (growing) repos. Deferred caching strategies (S3 binary cache populated per-repo by CI, or EFS warm store) are **out of scope** for C1.
3. **Non-root, single-user Nix.** The runtime user is uid 10001, so Nix is installed single-user with `/nix` owned by that uid (no daemon). This is the one piece with real implementation risk and is validated by an explicit test.

---

## 2. Worker image changes (cica `Dockerfile`, runtime stage)

1. **Install Nix (pinned), single-user.** In the runtime stage, install a pinned Nix version into `/nix` and make it usable by uid 10001:
   - Create `/nix` and `chown` it to uid 10001 so the non-root runtime user can realize derivations (single-user mode, no `nix-daemon`).
   - Put Nix on the runtime user's `PATH` (profile script under `/nix/var/nix/profiles/...` or a symlink into `/usr/local/bin`).
2. **Configure `nix.conf`** (system-wide, e.g. `/etc/nix/nix.conf`):
   - `experimental-features = nix-command flakes` (devenv requires flakes).
   - `substituters = https://cache.nixos.org https://devenv.cachix.org`
   - `trusted-public-keys = cache.nixos.org-1:... devenv.cachix.org-1:...` (the real public keys).
   - For single-user as uid 10001, mark that uid as a trusted user so the substituters/keys are honored.
3. **Install devenv** in the image (e.g. `nix profile install nixpkgs#devenv` or the devenv-recommended install), so the devenv tool and its `nixpkgs` base closure are already present. This is the only thing pre-warmed — nothing repo-specific. Only a *target repo's* delta downloads at runtime.
4. Verify at build time: `devenv version` succeeds.

Everything else in the image is unchanged. The image grows by roughly a few hundred MB to ~1 GB (Nix + devenv base closure) — accepted.

## 3. Runtime behavior

When the agent (C3) has cloned a repo containing `devenv.{nix,yaml,lock}`, it runs commands inside the repo's environment via `devenv shell -- <cmd>` (build/test/lint), or `devenv test` where the repo defines tests. The first such call on a fresh worker downloads the repo's closure from the configured caches (cold, once per worker / per turn); subsequent commands in the same turn reuse the warm local store. Repos without a devenv config fall back to whatever the image provides (git/bun) — C2's per-repo `AGENTS.md` will state which path applies.

## 4. Validation / acceptance (the de-risking test)

The non-root Nix-in-container behavior is proven by a test that runs **as uid 10001 in the built worker image** and realizes a Nix derivation + exercises devenv:

- Build the worker image.
- `docker run --user 10001 <image>` a script that, in a writable temp dir, either (a) runs `devenv init` to generate a minimal `devenv.{nix,yaml}` then `devenv shell -- <true/echo>`, or (b) runs a minimal `devenv shell` against a tiny committed fixture config; assert exit 0 and that a Nix-provided binary runs (e.g. a `pkgs.hello`/`coreutils` command resolved through the shell).
- The test asserts: Nix can build/substitute as the non-root user (`/nix` writable, substituters honored) and `devenv shell` enters successfully.

This runs in CI (a new image-based job alongside the existing docker-flow job) and can also be confirmed once on a live Fargate worker. A heavier end-to-end check (clone a real Root devenv repo and run its tests on a live worker) is the C1 sign-off but belongs to the live step, not unit CI (it needs repo access — C2).

## 5. Definition of done
- Worker image installs pinned Nix (single-user, `/nix` owned by uid 10001) + devenv, with `nix.conf` enabling flakes + the public substituters/keys; `devenv version` works at build time.
- The non-root acceptance test passes: as uid 10001 in the built image, a Nix derivation is realized and `devenv shell` runs a command successfully.
- Existing worker behavior is unchanged (git/bun/claude-code still work; the image still runs as uid 10001).
- Image size increase is documented; no private cache/EFS/baking introduced.

## 6. Out of scope (later)
- **C1 optimizations (deferred):** S3-backed private Nix binary cache populated per-repo by CI (`post-build-hook`), EFS warm `/nix` store, or baking repo closures — any of which removes the cold first-use cost. To be designed when first-use latency becomes a real pain.
- **C2:** repo access/PR creds for target repos + per-repo `AGENTS.md` conventions (build/test/devenv commands the agent reads).
- **C3:** reworking `linear-agent` into the devenv-aware ticket→PR flow with a test-before-PR loop.
- Repos without a devenv config (broader toolchain provisioning) beyond the image's existing tools.

## 7. Testing approach
- **Image/CI test (primary):** the non-root devenv acceptance from §4, as a CI job that builds the worker image and runs the uid-10001 devenv check. This is the de-risking gate for the non-root Nix install.
- **Build-time smoke:** `devenv version` in the Docker build.
- **Live (sign-off):** on one Fargate worker, `devenv shell -- <cmd>` against a real Root devenv repo (gated on C2 repo access) — confirms cold download + real-repo build work end-to-end.
