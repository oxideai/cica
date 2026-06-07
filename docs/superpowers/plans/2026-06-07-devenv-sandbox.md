# devenv-Capable Worker Sandbox Implementation Plan (Phase C1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Install Nix + devenv into the worker image (single-user, runnable by the non-root uid 10001) so the agent can `devenv shell` a cloned repo and build/test it, with deps pulled cold from the public Nix caches at runtime.

**Architecture:** Add a Nix (single-user, `/nix` owned by uid 10001) + devenv layer to the cica `Dockerfile` runtime stage, with `/etc/nix/nix.conf` enabling flakes + the public substituters. Prove the non-root install works via a CI test that runs **as uid 10001 in the built image**, realizes a Nix derivation, and enters a `devenv shell`.

**Tech Stack:** Docker, Nix (single-user), devenv, GitHub Actions.

**Spec:** `cica/docs/superpowers/specs/2026-06-07-devenv-sandbox-design.md`
**Branch:** `feat/devenv-sandbox` (already created in `/Users/dcvz/Github/cica`).

---

## File Structure

- **Modify** `Dockerfile` — runtime stage: create the cica user earlier, add Nix + devenv, `/etc/nix/nix.conf`, PATH.
- **Create** `tests/fixtures/devenv-smoke/devenv.nix` + `tests/fixtures/devenv-smoke/devenv.yaml` — a tiny deterministic devenv project for the acceptance test.
- **Create** `scripts/test-devenv-sandbox.sh` — runs the non-root acceptance against a built image.
- **Modify** `.github/workflows/ci.yml` — add the acceptance step to the `docker-flow` job (reuses its built image).

---

## Task 1: Add Nix + devenv to the worker image (non-root, single-user)

**Files:** Modify `Dockerfile`

The runtime stage (final `FROM ubuntu:24.04`, lines ~31–75) installs tools and creates the `cica` user (uid 10001) at the end. Nix must be installed *as* that user (single-user, `/nix` owned by uid 10001), so the user is created **before** the Nix block, and the final `chown` of `/data/cica` stays at the end.

**Note on fiddliness:** the exact Nix-installer invocation for non-root single-user in a container can need small tweaks. The contract is: at build time `devenv version` succeeds, and the Task 2 test passes. The commands below are the known-good approach; if the installer needs an extra flag, adjust minimally to satisfy those two gates — do not switch to a daemon/multi-user install.

- [ ] **Step 1: Create the cica user before the Nix block.** In the runtime stage, replace the existing trailing user block:
```dockerfile
RUN useradd --create-home --uid 10001 cica \
 && chown -R cica:cica /data/cica
```
with just the chown (the user is now created earlier):
```dockerfile
RUN chown -R cica:cica /data/cica
```
and add, immediately after the `apt-get install ... git` block (around line 34):
```dockerfile
# Non-root runtime user (uid 10001). Created early because Nix is installed
# single-user, owned by this uid (claude-code refuses root; the Fargate task-def
# runs as `cica`).
RUN useradd --create-home --uid 10001 cica
```

- [ ] **Step 2: Write the system Nix config.** Add (after the user is created, still as root):
```dockerfile
# Nix config: flakes + public binary caches (cold builds are downloads, not
# from-source). Single-user store owned by uid 10001.
RUN mkdir -p /etc/nix \
 && printf '%s\n' \
      'experimental-features = nix-command flakes' \
      'substituters = https://cache.nixos.org https://devenv.cachix.org' \
      'trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=' \
      'trusted-users = root cica' \
    > /etc/nix/nix.conf
```

- [ ] **Step 3: Install Nix single-user as uid 10001 + devenv.** Add:
```dockerfile
# -- Nix (single-user, store owned by uid 10001) + devenv --
RUN mkdir -m 0755 /nix && chown cica:cica /nix
USER cica
ENV USER=cica HOME=/home/cica
RUN curl -L https://releases.nixos.org/nix/nix-2.24.10/install | sh -s -- --no-daemon --no-modify-profile \
 && . /home/cica/.nix-profile/etc/profile.d/nix.sh \
 && nix profile install nixpkgs#devenv \
 && devenv version
USER root
# Make nix + devenv available to non-login shells (the agent runs non-login bash).
ENV PATH="/home/cica/.nix-profile/bin:${PATH}"
```

- [ ] **Step 4: Keep the binary copy + entrypoint as-is.** Ensure `COPY --from=binsrc /cica /usr/local/bin/cica`, the trailing `RUN chown -R cica:cica /data/cica`, and `ENTRYPOINT ["cica"]` remain after the Nix block. (Order: apt → user → nix.conf → Nix+devenv → bun/cursor/claude-code → COPY cica → chown → ENTRYPOINT. Bun/cursor/claude-code installs may stay where they are as long as they're before the final chown; the Nix block can sit right after the user creation.)

- [ ] **Step 5: Build the image and confirm devenv is present.**

Run: `cd /Users/dcvz/Github/cica && docker build -t cica-worker:devenv-test .`
Expected: build succeeds; the `devenv version` line in Step 3 printed a version during build (no error). If the Nix install step errors, iterate on the installer flags (e.g. ensure `/nix` ownership, `USER`/`HOME` env) until `devenv version` succeeds — keep it single-user.

- [ ] **Step 6: Commit**

```bash
git add Dockerfile
git commit -m "feat(worker): install Nix + devenv (single-user, uid 10001) in the worker image"
```

---

## Task 2: Non-root acceptance test (fixture + script + CI)

**Files:** Create `tests/fixtures/devenv-smoke/devenv.nix`, `tests/fixtures/devenv-smoke/devenv.yaml`, `scripts/test-devenv-sandbox.sh`; modify `.github/workflows/ci.yml`

- [ ] **Step 1: Create a tiny deterministic devenv fixture.**

`tests/fixtures/devenv-smoke/devenv.yaml`:
```yaml
inputs:
  nixpkgs:
    url: github:NixOS/nixpkgs/nixpkgs-unstable
```

`tests/fixtures/devenv-smoke/devenv.nix`:
```nix
{ pkgs, ... }:
{
  packages = [ pkgs.hello ];
}
```

- [ ] **Step 2: Write the acceptance script.** Create `scripts/test-devenv-sandbox.sh`:
```bash
#!/usr/bin/env bash
# Proves the worker image can build Nix derivations and enter a devenv shell
# AS THE NON-ROOT runtime user (uid 10001) — the C1 de-risking gate.
# Usage: IMAGE=cica-worker:devenv-test scripts/test-devenv-sandbox.sh
set -euo pipefail
IMAGE="${IMAGE:?set IMAGE to the built worker image tag}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Run as uid 10001, non-login bash (mirrors how the agent runs commands).
docker run --rm --user 10001 \
  -v "$REPO_ROOT/tests/fixtures/devenv-smoke:/work:ro" \
  "$IMAGE" bash -c '
    set -euo pipefail
    echo "== nix/devenv present =="
    nix --version
    devenv version
    echo "== realize a derivation as non-root (proves /nix writable + substituters) =="
    nix build --no-link --print-out-paths nixpkgs#hello
    echo "== devenv shell enters and runs a command =="
    # copy fixture to a writable dir (devenv writes .devenv/, lockfile)
    cp -r /work /tmp/devenv-smoke && cd /tmp/devenv-smoke
    devenv shell -- hello
  ' | tee /tmp/devenv-test.out

grep -q "Hello, world!" /tmp/devenv-test.out
echo "DEVENV SANDBOX OK"
```
Make it executable: `chmod +x scripts/test-devenv-sandbox.sh`.

- [ ] **Step 3: Run it locally against the Task 1 image.**

Run: `cd /Users/dcvz/Github/cica && IMAGE=cica-worker:devenv-test scripts/test-devenv-sandbox.sh`
Expected: ends with `DEVENV SANDBOX OK` (nix + devenv versions print, `nix build` returns a store path, `devenv shell -- hello` prints `Hello, world!`). This is the core proof that single-user Nix works for the non-root user. If `devenv shell` can't write or nix can't substitute, fix Task 1 (perms/PATH/nix.conf) until this passes.

- [ ] **Step 4: Wire the test into the `docker-flow` CI job.** In `.github/workflows/ci.yml`, the `docker-flow` job builds `cica-worker:latest`. Add a step after the existing "Build worker image" step:
```yaml
      - name: devenv sandbox test (non-root)
        run: IMAGE=cica-worker:latest scripts/test-devenv-sandbox.sh
```
(Place it before or after the existing docker integration test; it only needs the built `cica-worker:latest` image.)

- [ ] **Step 5: Commit**

```bash
git add tests/fixtures/devenv-smoke/ scripts/test-devenv-sandbox.sh .github/workflows/ci.yml
git commit -m "test(worker): non-root devenv sandbox acceptance (fixture + CI step)"
```

---

## Task 3: Verify + document sign-off

**Files:** none (validation); optional note in the spec

- [ ] **Step 1: Full local gate.**

Run:
```bash
cd /Users/dcvz/Github/cica
docker build -t cica-worker:devenv-test .
IMAGE=cica-worker:devenv-test scripts/test-devenv-sandbox.sh
```
Expected: `DEVENV SANDBOX OK`.

- [ ] **Step 2: Note image-size delta.**

Run: `docker images cica-worker:devenv-test --format '{{.Size}}'`
Record the size in the PR description (expected increase of a few hundred MB–~1 GB from the Nix + devenv base closure — accepted per the spec).

- [ ] **Step 3: Record the deferred live sign-off.** In the PR description, note: the heavier end-to-end check — on a live Fargate worker, clone a real Root devenv repo and run `devenv shell -- <its test>` — is gated on **C2** (repo access) and is the C1 live sign-off, not part of this CI. No code change here.

---

## Self-review notes for the implementer
- The **single-user, non-root Nix install is the only real risk.** The two gates are `devenv version` at build time (Task 1 Step 5) and `scripts/test-devenv-sandbox.sh` ending in `DEVENV SANDBOX OK` (Task 2 Step 3). Iterate the installer invocation to satisfy both; stay single-user (no daemon).
- Do **not** add any private cache / EFS / baked repo closures — that's deferred (spec §6).
- The local DockerLauncher runs the image as **root** by default; the devenv flow targets the **cloud worker (uid 10001)**, which the Fargate task-def selects with `user: cica`. The test deliberately runs `--user 10001` to match that.
- Keep the existing bun/cursor/claude-code/entrypoint behavior intact — only add the Nix layer + reorder user creation.
- CI: the test reuses `docker-flow`'s already-built image, so it adds the `devenv shell` download cost (small fixture) but not a second image build.
