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

# A non-root user (uid 10001) owning /data/cica. The image default stays root
# (so the local DockerLauncher's bind-mounted state-store stays writable); the
# runtime opts into this user where it matters — e.g. the Fargate task-def sets
# `user: cica`, because claude-code refuses --dangerously-skip-permissions under
# root and the cloud worker's state is in S3 (no host mount to worry about).
RUN useradd --create-home --uid 10001 cica \
 && chown -R cica:cica /data/cica

ENTRYPOINT ["cica"]
