#!/usr/bin/env bash
# Proves the worker image can build Nix derivations and enter a devenv shell
# AS THE NON-ROOT runtime user (uid 10001) — the C1 de-risking gate.
# Usage: IMAGE=cica-worker:devenv-test scripts/test-devenv-sandbox.sh
set -euo pipefail
IMAGE="${IMAGE:?set IMAGE to the built worker image tag}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker run --rm --user 10001 \
  --entrypoint bash \
  -v "$REPO_ROOT/tests/fixtures/devenv-smoke:/work:ro" \
  "$IMAGE" -c '
    set -euo pipefail
    echo "== nix/devenv present =="
    nix --version
    devenv version
    echo "== realize a derivation as non-root =="
    nix build --no-link --print-out-paths nixpkgs#hello
    echo "== devenv shell enters and runs a command =="
    cp -r /work /tmp/devenv-smoke && cd /tmp/devenv-smoke
    devenv shell -- hello
  ' | tee /tmp/devenv-test.out
grep -q "Hello, world!" /tmp/devenv-test.out
echo "DEVENV SANDBOX OK"
