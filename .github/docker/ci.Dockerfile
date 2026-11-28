# syntax=docker/dockerfile:1.7

# Multi-target Linux x64 Forgejo Actions runner image:
# - `x64` (default): full Linux x86_64/aarch64 server, Linux x86_64 desktop,
#   Windows x86_64/aarch64 MSVC (cargo-xwin + Wine + KVM), macOS cross-packaging
#   (rcodesign + DMG tools), Wasm (wasm32-wasip2), and cargo-nextest.
# - `arm64-desktop`: shares the compiled toolchain base with arm64 multiarch
#   GTK/WebKit2GTK sysroot for cross-compiling & bundling Linux aarch64 AppImages.

FROM ubuntu:24.04 AS base

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.99.1
ARG NODE_VERSION=24.14.0
ARG BUN_VERSION=1.4.2
ARG TASK_VERSION=3.52.0
ARG UV_VERSION=0.11.28
ARG CARGO_XWIN_VERSION=0.22.0
ARG PATCH_CRATE_VERSION=0.1.13
ARG SCCACHE_VERSION=0.10.0
ARG CARGO_NEXTEST_VERSION=0.9.152
ARG APPLE_CODESIGN_VERSION=0.29.0
ARG APT_MIRROR=""
ARG APT_MIRROR_PORTS=""
ARG RUSTUP_DIST_SERVER=""
ARG RUSTUP_UPDATE_ROOT=""
ARG NODE_MIRROR=""

# Keep CARGO_HOME unset at runtime so cargo uses $HOME/.cargo (matching standard actions/cache paths),
# while preinstalled rustup/cargo binaries live under /usr/local/cargo/bin on PATH.
# Do not bake UV_DEFAULT_INDEX here: overriding the default index breaks `uv run --locked` against uv.lock.
ENV RUSTUP_HOME=/usr/local/rustup \
    BUN_INSTALL=/usr/local/bun \
    XWIN_CACHE_DIR=/usr/local/xwin \
    XWIN_VERSION=16 \
    SCCACHE_CACHE_SIZE=5G \
    CARGO_INCREMENTAL=0 \
    WINEDEBUG=-all \
    PATH=/usr/local/cargo/bin:/usr/local/bun/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Configure Ubuntu apt mirror (defaults to archive.ubuntu.com; override via APT_MIRROR for domestic runners).
# zstd is required by actions/cache@v6 to use multi-threaded `.tzst` archives instead of single-threaded gzip.
RUN set -eux; \
    amd64_uri="${APT_MIRROR:-http://archive.ubuntu.com/ubuntu/}"; \
    printf '%s\n' \
      'Types: deb' \
      "URIs: ${amd64_uri}" \
      'Suites: noble noble-updates noble-security' \
      'Components: main universe restricted multiverse' \
      'Architectures: amd64' \
      'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/ubuntu.sources; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      ca-certificates curl git jq unzip zip tar xz-utils zstd bzip2 patch file \
      build-essential pkg-config cmake ninja-build clang llvm lld libclang-dev nasm \
      python3 python3-pip \
      libssl-dev \
      patchelf xdg-utils squashfs-tools minisign \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross; \
    rm -rf /var/lib/apt/lists/*

# Install Node.js 24 (required by actions/checkout@v7, actions/cache@v6, etc.).
RUN set -eux; \
    node_base="${NODE_MIRROR:-https://nodejs.org/dist}"; \
    curl -fsSL "${node_base%/}/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ --strip-components=1 -C /usr/local; \
    node -v; npm -v

# Install pinned Rust toolchain + cross-compilation targets.
RUN set -eux; \
    export CARGO_HOME=/usr/local/cargo; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --default-toolchain "${RUST_VERSION}" --profile minimal --component rustfmt,clippy; \
    rustup target add \
      wasm32-wasip2 \
      aarch64-unknown-linux-gnu \
      x86_64-pc-windows-msvc \
      aarch64-pc-windows-msvc \
      aarch64-apple-darwin \
      x86_64-apple-darwin; \
    cargo install --locked --version "${PATCH_CRATE_VERSION}" patch-crate; \
    cargo install --locked --version "${CARGO_XWIN_VERSION}" cargo-xwin; \
    rm -rf "${CARGO_HOME}/registry/cache" "${CARGO_HOME}/registry/src"

# Install pinned Bun, Task, uv, sccache, cargo-nextest, and rcodesign.
RUN set -eux; \
    curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}"; \
    curl -fsSL "https://github.com/go-task/task/releases/download/v${TASK_VERSION}/task_linux_amd64.tar.gz" \
      | tar -xz -C /usr/local/bin task; \
    curl -fsSL "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-x86_64-unknown-linux-gnu.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin uv-x86_64-unknown-linux-gnu/uv uv-x86_64-unknown-linux-gnu/uvx; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache"; \
    curl -fsSL "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${CARGO_NEXTEST_VERSION}/cargo-nextest-${CARGO_NEXTEST_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin cargo-nextest; \
    curl -fsSL "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${APPLE_CODESIGN_VERSION}/apple-codesign-${APPLE_CODESIGN_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin "apple-codesign-${APPLE_CODESIGN_VERSION}-x86_64-unknown-linux-musl/rcodesign"; \
    rustc --version; cargo --version; cargo xwin --version; cargo nextest --version; sccache --version; rcodesign --version; bun --version; task --version; uv --version

WORKDIR /workspace

# Linux aarch64 desktop cross-compilation image: keeps amd64 GTK/WebKit2GTK out of `base`
# so multiarch arm64 GTK/WebKit2GTK development packages install without file/gir conflicts.
FROM base AS arm64-desktop

ARG DEBIAN_FRONTEND=noninteractive
ARG APT_MIRROR_PORTS=""

RUN set -eux; \
    arm64_uri="${APT_MIRROR_PORTS:-http://ports.ubuntu.com/ubuntu-ports/}"; \
    dpkg --add-architecture arm64; \
    printf '%s\n' \
      'Types: deb' \
      "URIs: ${arm64_uri}" \
      'Suites: noble noble-updates noble-security' \
      'Components: main universe restricted multiverse' \
      'Architectures: arm64' \
      'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/ubuntu-arm64-ports.sources; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev:arm64 \
      libgtk-3-dev:arm64 \
      libayatana-appindicator3-dev:arm64 \
      librsvg2-dev:arm64 \
      libssl-dev:arm64; \
    rm -rf /var/lib/apt/lists/*

# Default target (`x64`): full Linux x86_64 desktop, Windows xwin/Wine/KVM, and macOS cross-packaging.
FROM base AS x64

ARG DEBIAN_FRONTEND=noninteractive
ARG PREFETCH_XWIN=1

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev \
      libgtk-3-dev \
      libayatana-appindicator3-dev \
      librsvg2-dev \
      libxml2-dev uuid-dev zlib1g-dev libbz2-dev liblzma-dev hfsprogs \
      nsis wine64 wine \
      qemu-system-x86 qemu-utils genisoimage openssh-client; \
    rm -rf /var/lib/apt/lists/*; \
    makensis -VERSION; wine --version; qemu-system-x86_64 --version; qemu-img --version; genisoimage --version; ssh -V

# Pre-populate Windows MSVC CRT + Windows SDK cache (x86_64 & aarch64) so CI jobs
# do not depend on live Microsoft download availability at build time.
RUN set -eux; \
    if [ "${PREFETCH_XWIN}" = "1" ]; then \
      export CARGO_HOME=/usr/local/cargo; \
      mkdir -p "${XWIN_CACHE_DIR}"; \
      cargo install --locked xwin; \
      xwin --accept-license --cache-dir "${XWIN_CACHE_DIR}" --manifest-version "${XWIN_VERSION}" --arch x86_64,aarch64 download; \
      rm -rf "${CARGO_HOME}/registry/cache" "${CARGO_HOME}/registry/src"; \
    fi
