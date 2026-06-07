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
 && apt-get install -y --no-install-recommends ca-certificates curl unzip git xz-utils \
 && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (uid 10001). Created early because Nix is installed
# single-user, owned by this uid (claude-code refuses root; Fargate runs `cica`).
RUN useradd --create-home --uid 10001 cica

# Pin cica's data dir to /data/cica (ProjectDirs honors XDG_CONFIG_HOME on Linux).
ENV XDG_CONFIG_HOME=/data

RUN mkdir -p \
      /data/cica/internal/deps/bun \
      /data/cica/internal/deps/cursor-cli \
      /data/cica/internal/deps/claude-code \
      /data/cica/internal/claude-home \
      /data/cica/internal/cursor-home

# Nix: flakes + public binary caches (cold builds are downloads, not from-source).
RUN mkdir -p /etc/nix \
 && printf '%s\n' \
      'experimental-features = nix-command flakes' \
      'substituters = https://cache.nixos.org https://devenv.cachix.org' \
      'trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=' \
      'trusted-users = root cica' \
    > /etc/nix/nix.conf

# -- Nix (single-user, store owned by uid 10001) + devenv --
RUN mkdir -m 0755 /nix && chown cica:cica /nix
USER cica
ENV USER=cica HOME=/home/cica
RUN curl -L https://releases.nixos.org/nix/nix-2.24.10/install | sh -s -- --no-daemon --no-modify-profile \
 && . /home/cica/.nix-profile/etc/profile.d/nix.sh \
 && nix profile install nixpkgs#devenv \
 && devenv version
USER root
# nix + devenv on PATH for non-login shells (the agent runs non-login bash) and
# HOME for the runtime cica user (needed to find ~/.nix-profile).
ENV PATH="/home/cica/.nix-profile/bin:${PATH}"

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

RUN chown -R cica:cica /data/cica

ENTRYPOINT ["cica"]
