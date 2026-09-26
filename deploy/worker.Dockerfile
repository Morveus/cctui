# cctui worker image (contract v1) — the execution environment dispatchers
# (the docker / kubernetes dispatchers) spawn per session. Bundles:
#   - the claude code CLI
#   - the codex CLI
#   - the cctui-daemon binary (session observability)
#   - the cctui sandbox toolchain: cctui-guard-proxy (egress allow-list),
#     cctui-supervisor (landlock + seccomp + cap-drop exec wrapper), and
#     cctui-guard (markdown-driven workflow guard daemon)
#
# It is **non-enrolled**: NO credentials and NO machine identity are baked in.
# Identity arrives at spawn time as env injected by the dispatcher:
#   CCTUI_URL           — the cctui-server base URL the daemon dials out to
#   CCTUI_MACHINE_KEY   — the shared machine key (the daemon runs without enroll)
#   SESSION_ID          — the pre-minted session to register
#   TASK_PAYLOAD_JSON   — the dispatch payload
#   REPLY_URL, TASK_NAME (optional)
#
# The daemon resolves CCTUI_MACHINE_KEY + CCTUI_URL from the environment
# (Config::from_env) and runs in ephemeral, --no-auto-update mode: the worker
# is short-lived and re-fetched on spawn, so there is nothing to self-update.
#
# This is a LEAN base. Heavy toolchains (Rust, Go, pnpm stores, dockerd) belong
# in **derived org images** (`FROM ghcr.io/<org>/cctui-worker`), never here —
# the base only ships what every worker needs to boot, sandbox, and talk git.
#
# See docs/worker-contract.md for the full env / mount / capability contract.
#
# ⚠️ This repo is PUBLIC. Keep it free of any private/homelab registries,
# hosts, or namespaces — neutral placeholders only.

# ── Builder: compile the worker binaries ────────────────────────────────────
# Match the runtime's glibc (bookworm-slim ships glibc 2.36) by building on the
# bookworm-based rust image, same as deploy/Dockerfile.
FROM rust:1.97.1-slim-bookworm@sha256:b001fed8c602fe3126bfee18c7afa14fe58dc855ce1d0cdfb4ac3ee7d6361a1c AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY migrations/ migrations/
# cctui-daemon's service.rs include_str!()s these unit templates at compile time.
COPY packaging/ packaging/

# sqlx runs in offline mode so no database is needed at build time.
ENV SQLX_OFFLINE=true
RUN cargo build --release \
        -p cctui-daemon \
        -p cctui-guard-proxy \
        -p cctui-supervisor \
        -p cctui-guard

# ── Runtime: claude code + codex + cctui binaries ───────────────────────────
# Every bundled CLI is a native binary; node is here only so context packs can
# run npx-based MCP servers. Heavier JS tooling (pnpm stores, a managed
# toolchain) still belongs in derived org images, e.g. under /opt/mise.
FROM debian:bookworm-slim@sha256:63a496b5d3b99214b39f5ed70eb71a61e590a77979c79cbee4faf991f8c0783e

# Base tooling kept deliberately lean:
#   ca-certificates          — TLS trust roots for the daemon's rustls stack.
#   git, git-lfs            — repo work and large-file fetches.
#   ripgrep                 — what claude code shells out to for search.
#   gnupg                   — sidecar gpg-agent (holds the signing key) + the
#                             worker-side gpg client that signs over the
#                             forwarded extra socket.
#   jq                      — payload unpack + result-callback synthesis.
#   curl                    — context-pack token auth, result callback, health.
#   rsync                   — warm-repo workspace fallback when overlayfs is off.
#   iptables                — transparent-mode egress REDIRECT to the proxy.
#   openssh-client          — git over SSH for credentialed clones.
#   gh                      — GitHub CLI for token auth + PR work.
#   xz-utils                — unpacks the node tarball below.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        git-lfs \
        gnupg \
        iptables \
        jq \
        openssh-client \
        ripgrep \
        rsync \
        xz-utils \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# Node — the runtime only, from the official nodejs.org tarball, checksum-verified
# against the release's SHASUMS256.txt. Present so context packs can configure
# npx-based MCP servers (docs/context-packs.md) and so derived images inherit a
# node on PATH; none of the bundled CLIs depend on it. Pinning the tarball rather
# than tracking a `node:` base image tag keeps the major explicit.
ARG NODE_VERSION=24.18.0
RUN arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
         amd64) target=linux-x64 ;; \
         arm64) target=linux-arm64 ;; \
         *) echo "node: unsupported arch '$arch'" >&2; exit 1 ;; \
       esac \
    && base="https://nodejs.org/dist/v${NODE_VERSION}" \
    && tarball="node-v${NODE_VERSION}-${target}.tar.xz" \
    && curl -fsSL "${base}/${tarball}" -o /tmp/node.tar.xz \
    && curl -fsSL "${base}/SHASUMS256.txt" -o /tmp/node.sums \
    && sha="$(awk -v f="$tarball" '$2==f{print $1}' /tmp/node.sums)" \
    && [ -n "$sha" ] || { echo "node: no checksum for ${tarball}" >&2; exit 1; } \
    && echo "${sha}  /tmp/node.tar.xz" | sha256sum -c - \
    && mkdir -p /usr/local/node \
    && tar -xJf /tmp/node.tar.xz -C /usr/local/node --strip-components=1 \
    && rm /tmp/node.tar.xz /tmp/node.sums \
    # include/ is node-gyp's C++ headers for building native addons, which needs a
    # compiler this image does not ship; share/ is man pages and docs.
    && rm -rf /usr/local/node/include /usr/local/node/share \
    && ln -s /usr/local/node/bin/node /usr/local/bin/node \
    && ln -s /usr/local/node/bin/npm  /usr/local/bin/npm \
    && ln -s /usr/local/node/bin/npx  /usr/local/bin/npx \
    && node --version && npx --version

# Claude Code — the per-platform *native* binary pulled straight from Anthropic's
# release CDN (what claude.ai/install.sh fetches), NOT the npm package: the
# native binary never invokes node, so it stays independent of whatever node a
# derived image puts on PATH. Checksum-verified against the release manifest,
# mirroring codex below.
# A concrete x.y.z MINIMUM, not an exact pin: derived images (the harbor worker
# bake) refetch the harness, so the guarantee is only "never older than this".
# scripts/check-claude-version-drift.sh enforces the floor against the installed
# binary and reports when it falls behind upstream; the release workflow runs it
# via scripts/check-worker-image-harness.sh against this image before pushing,
# which is the only place a real binary exists. `latest`/`stable` still resolve
# if passed explicitly as a build arg.
ARG CLAUDE_CODE_VERSION=2.1.258
RUN base="https://downloads.claude.ai/claude-code-releases" \
    && case "$(dpkg --print-architecture)" in \
         amd64) platform=linux-x64 ;; \
         arm64) platform=linux-arm64 ;; \
         *) echo "claude: unsupported arch" >&2; exit 1 ;; \
       esac \
    && case "${CLAUDE_CODE_VERSION}" in \
         latest|stable) version="$(curl -fsSL "${base}/${CLAUDE_CODE_VERSION}")" ;; \
         *) version="${CLAUDE_CODE_VERSION}" ;; \
       esac \
    && echo "${version}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+' \
    && sha="$(curl -fsSL "${base}/${version}/manifest.json" \
        | jq -er --arg p "$platform" '.platforms[$p].checksum')" \
    && curl -fsSL "${base}/${version}/${platform}/claude" -o /usr/local/bin/claude \
    && echo "${sha}  /usr/local/bin/claude" | sha256sum -c - \
    && chmod 0755 /usr/local/bin/claude \
    && claude --version

# Codex — native static-musl binary from the GitHub release, NOT the npm package.
# The npm codex is a node entrypoint, so in derived images that put a node shim
# on PATH (e.g. mise managing the toolchain) launching it resolves `node`
# through the shim and can fail before codex starts (the
# "mise ERROR Permission denied (os error 13)" seen in the acme fat image).
# The standalone binary has no node dependency and sidesteps that entirely.
# Checksum-verified.
#
# Model provider: codex IGNORES OPENAI_API_KEY / OPENAI_BASE_URL env and reads
# its provider only from ~/.codex/config.toml. Do NOT bake a static config here —
# it would clobber codex's own runtime writes (trust_level) and pin a stale
# base_url. The entrypoint's phase_codex_config MERGES the cctui gateway provider
# in at runtime from the injected OPENAI_* env.
# A concrete x.y.z MINIMUM, not an exact pin: derived images (the harbor worker
# bake) refetch the harness, so the guarantee is only "never older than this".
# Must equal contract::CODEX_MIN_VERSION
# (crates/cctui-daemon/src/adapters/codex/contract.rs), the build the retained
# JSON Schema was generated from; scripts/check-codex-version-drift.sh enforces
# both that and the floor against any installed binary.
ARG CODEX_VERSION=0.153.4
RUN arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
         amd64) target=x86_64-unknown-linux-musl;  sha=f479424eca092484dc40d87ae28c44f4cc40234a60045d6131e493800d814a30 ;; \
         arm64) target=aarch64-unknown-linux-musl; sha=5cda6182bd94c3a30f2eb63a495489ebf7f691fddb14d70f48c6c1a5071b6cde ;; \
         *) echo "codex: unsupported arch '$arch'" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${target}.tar.gz" \
        -o /tmp/codex.tar.gz \
    && echo "${sha}  /tmp/codex.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/codex.tar.gz -C /usr/local/bin \
    && mv "/usr/local/bin/codex-${target}" /usr/local/bin/codex \
    && chmod 0755 /usr/local/bin/codex \
    && rm /tmp/codex.tar.gz \
    && codex --version

# opencode — third harness (https://github.com/anomalyco/opencode), driven by the
# daemon's opencode adapter over `opencode serve`. Pinned + checksum-verified:
# the release carries no SHA256SUMS asset, so the digests are recorded per arch
# alongside the version and must be refreshed together with it.
#
# NO model id and NO opencode config belong in this image: the adapter writes a
# per-session opencode.json into the session's ephemeral HOME from the dispatch
# payload + gateway-minted FIREWORKS_* env.
# Keep OPENCODE_VERSION in lockstep with client::OPENCODE_PINNED_VERSION
# (crates/cctui-daemon/src/adapters/opencode/client.rs).
ARG OPENCODE_VERSION=1.18.7
RUN arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
         amd64) target=x64;   sha=cb5d9d6d2f8fbef0a9c975ed4494f73b2a62f4e4ffd508bcc3212da4fa76c3da ;; \
         arm64) target=arm64; sha=6c791e453c2ca03ee3dea09ebd16bfdfac4837e45d344a1487cd196b80090fc7 ;; \
         *) echo "opencode: unsupported arch '$arch'" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "https://github.com/anomalyco/opencode/releases/download/v${OPENCODE_VERSION}/opencode-linux-${target}.tar.gz" \
        -o /tmp/opencode.tar.gz \
    && echo "${sha}  /tmp/opencode.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/opencode.tar.gz -C /usr/local/bin opencode \
    && chmod 0755 /usr/local/bin/opencode \
    && rm /tmp/opencode.tar.gz \
    && opencode --version

# Worker user (uid 1000). The container starts as root only to bootstrap the
# sandbox (iptables, overlayfs, context pack), then cctui-supervisor setuids to
# this user before exec'ing the daemon. A real home keeps per-session agent
# state (~/.claude, ~/.codex, ~/.mcp.json, ~/.npmrc, ~/.gnupg) writable.
RUN groupadd --gid 1000 worker \
    && useradd --uid 1000 --gid 1000 --home-dir /home/worker --create-home --shell /bin/bash worker \
    && chmod 0755 /home/worker

COPY --from=builder /app/target/release/cctui-daemon       /usr/local/bin/cctui-daemon
COPY --from=builder /app/target/release/cctui-guard-proxy  /usr/local/bin/cctui-guard-proxy
COPY --from=builder /app/target/release/cctui-supervisor   /usr/local/bin/cctui-supervisor
COPY --from=builder /app/target/release/cctui-guard        /usr/local/bin/cctui-guard
COPY deploy/worker-entrypoint.sh   /usr/local/bin/cctui-worker-entrypoint
# worker-net-init — pod-netns iptables for the k8s sidecar mode: run
# from a NET_ADMIN init container so the worker container needs no privileged.
COPY deploy/worker-net-init.sh     /usr/local/bin/cctui-worker-net-init
# guard-proxy-entrypoint — sidecar boot wrapper: stands up a gpg-agent
# holding the signing key and forwards only its restricted --extra-socket, then
# exec's cctui-guard-proxy. Passthrough when no GPG_PRIVATE_KEY is present.
COPY deploy/guard-proxy-entrypoint.sh /usr/local/bin/cctui-guard-proxy-entrypoint

# Sandbox state dirs the entrypoint and proxy/guard write into. /opt/context is
# the context-pack mount target (read-only after fetch).
RUN mkdir -p /var/run/guard-proxy /var/run/workflow-guard /workspace /opt/context /opt/worker-entrypoint.d \
    && chmod +x /usr/local/bin/cctui-worker-entrypoint \
                /usr/local/bin/cctui-worker-net-init \
                /usr/local/bin/cctui-guard-proxy-entrypoint

# Contract marker: derived images and dispatchers can assert the wire contract.
LABEL dev.cctui.contract="1"

# HOME defaults to the worker home; the entrypoint runs as root first and the
# supervisor re-exports HOME=/home/worker for the dropped process.
ENV HOME=/home/worker
WORKDIR /home/worker

# Starts as root to bootstrap the sandbox; drops to uid 1000 via cctui-supervisor.
ENTRYPOINT ["/usr/local/bin/cctui-worker-entrypoint"]
