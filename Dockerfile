# ---- build stage ----
# Use Ubuntu 24.04 (glibc 2.39) — required by the pre-built ONNX Runtime
# native library bundled in ort-sys (fastembed dependency).
FROM ubuntu:24.04 AS build

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config libssl-dev cmake \
 && rm -rf /var/lib/apt/lists/*

# Install Rust via rustup (matches the toolchain in rust-toolchain.toml if present)
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /src
COPY . .
RUN cargo build --release --bin cica

# ---- runtime stage ----
# Match the build-stage glibc (Ubuntu 24.04 = glibc 2.39) so the cica binary
# and any bundled ONNX Runtime shared objects load without GLIBC version errors.
FROM ubuntu:24.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip git \
 && rm -rf /var/lib/apt/lists/*

# Pin cica's data dir to /data/cica (ProjectDirs honors XDG_CONFIG_HOME on Linux),
# so paths.base = /data/cica and the cursor workspace hash = md5("/data/cica").
ENV XDG_CONFIG_HOME=/data

# Pre-create the deps directories that setup.rs inspects.
# Layout mirrors config::paths() under /data/cica:
#   deps_dir        = /data/cica/internal/deps
#   bun_dir         = /data/cica/internal/deps/bun
#   cursor_cli_dir  = /data/cica/internal/deps/cursor-cli
#   claude_code_dir = /data/cica/internal/deps/claude-code
RUN mkdir -p \
      /data/cica/internal/deps/bun \
      /data/cica/internal/deps/cursor-cli \
      /data/cica/internal/deps/claude-code \
      /data/cica/internal/claude-home \
      /data/cica/internal/cursor-home

# -- Bun --
# find_bun() checks `which bun` first, so /usr/local/bin/bun is sufficient.
RUN curl -fsSL https://bun.sh/install | BUN_INSTALL=/usr/local bash \
 && bun --version

# -- Cursor CLI (cursor-agent) --
# find_cursor_cli() checks paths.cursor_cli_dir/cursor-agent first (before `which`).
# Download from the same URL cica uses at runtime:
#   https://downloads.cursor.com/lab/{VERSION}/{OS}/{ARCH}/agent-cli-package.tar.gz
# The tarball layout is dist-package/cursor-agent; strip one path component.
ARG CURSOR_CLI_VERSION=2026.01.28-fd13201
RUN curl -fsSL \
      "https://downloads.cursor.com/lab/${CURSOR_CLI_VERSION}/linux/x64/agent-cli-package.tar.gz" \
      -o /tmp/cursor-agent.tar.gz \
 && tar -xzf /tmp/cursor-agent.tar.gz --strip-components=1 \
      -C /data/cica/internal/deps/cursor-cli \
 && chmod +x /data/cica/internal/deps/cursor-cli/cursor-agent \
 && rm /tmp/cursor-agent.tar.gz

# -- Claude Code --
# find_claude_code() does NOT use `which`; it checks for:
#   claude_code_dir/node_modules/@anthropic-ai/claude-code/cli.js
# which resolves to:
#   /data/cica/internal/deps/claude-code/node_modules/@anthropic-ai/claude-code/cli.js
# Install the exact version cica expects via `bun add` into that directory.
ARG CLAUDE_CODE_VERSION=2.1.32
RUN cd /data/cica/internal/deps/claude-code \
 && bun add "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"

COPY --from=build /src/target/release/cica /usr/local/bin/cica

ENTRYPOINT ["cica"]
