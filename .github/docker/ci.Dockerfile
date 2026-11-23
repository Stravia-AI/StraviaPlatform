# syntax=docker/dockerfile:1.7

# Single Linux x64 Forgejo Actions runner image with full cross-compilation
# toolchains (Linux x86_64/aarch64, Windows x86_64/aarch64 MSVC via cargo-xwin,
# Wasm wasm32-wasip2) and Wine for Layer-1 Windows binary verification.
FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.99.1
ARG BUN_VERSION=1.4.2
ARG TASK_VERSION=3.52.0
ARG UV_VERSION=0.11.28
ARG CARGO_XWIN_VERSION=0.22.0
ARG PATCH_CRATE_VERSION=0.1.13
ARG SCCACHE_VERSION=0.10.0
ARG PREFETCH_XWIN=1
ARG APT_MIRROR=""
ARG APT_MIRROR_PORTS=""
ARG RUSTUP_DIST_SERVER=""
ARG RUSTUP_UPDATE_ROOT=""

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    BUN_INSTALL=/usr/local/bun \
    XWIN_CACHE_DIR=/usr/local/xwin \
    XWIN_VERSION=16 \
    SCCACHE_CACHE_SIZE=20G \
    CARGO_INCREMENTAL=0 \
    WINEDEBUG=-all \
    PATH=/usr/local/cargo/bin:/usr/local/bun/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Enable multiarch arm64 repositories for Linux aarch64 GTK/WebKitGTK cross-compilation.
RUN set -eux; \
    dpkg --add-architecture arm64; \
    amd64_uri="${APT_MIRROR:-http://archive.ubuntu.com/ubuntu/}"; \
    arm64_uri="${APT_MIRROR_PORTS:-http://ports.ubuntu.com/ubuntu-ports/}"; \
    cat <<EOF > /etc/apt/sources.list.d/ubuntu.sources
Types: deb
URIs: ${amd64_uri}
Suites: noble noble-updates noble-security
Components: main universe restricted multiverse
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: ${arm64_uri}
Suites: noble noble-updates noble-security
Components: main universe restricted multiverse
Architectures: arm64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
    apt-get update; \
    apt-get install -y --no-install-recommends \
      ca-certificates curl git jq unzip zip tar xz-utils file \
      build-essential pkg-config cmake clang llvm lld libclang-dev nasm \
      python3 python3-pip \
      libssl-dev \
      libwebkit2gtk-4.1-dev \
      libgtk-3-dev \
      libayatana-appindicator3-dev \
      librsvg2-dev \
      patchelf xdg-utils squashfs-tools minisign nsis \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
      libssl-dev:arm64 \
      libwebkit2gtk-4.1-dev:arm64 \
      libgtk-3-dev:arm64 \
      libayatana-appindicator3-dev:arm64 \
      librsvg2-dev:arm64 \
      wine64 wine \
      nodejs npm; \
    rm -rf /var/lib/apt/lists/*

# Install pinned Rust toolchain + cross-compilation targets.
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --default-toolchain "${RUST_VERSION}" --profile minimal --component rustfmt,clippy; \
    rustup target add \
      wasm32-wasip2 \
      aarch64-unknown-linux-gnu \
      x86_64-pc-windows-msvc \
      aarch64-pc-windows-msvc; \
    cargo install --locked --version "${PATCH_CRATE_VERSION}" patch-crate; \
    cargo install --locked --version "${CARGO_XWIN_VERSION}" cargo-xwin; \
    rm -rf "${CARGO_HOME}/registry/cache" "${CARGO_HOME}/registry/src"

# Install pinned Bun, Task, and uv.
RUN set -eux; \
    curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}"; \
    curl -fsSL "https://github.com/go-task/task/releases/download/v${TASK_VERSION}/task_linux_amd64.tar.gz" \
      | tar -xz -C /usr/local/bin task; \
    curl -fsSL "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-x86_64-unknown-linux-gnu.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin uv-x86_64-unknown-linux-gnu/uv uv-x86_64-unknown-linux-gnu/uvx; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache"; \
    rustc --version; cargo --version; cargo xwin --version; sccache --version; bun --version; task --version; uv --version; makensis -VERSION; wine64 --version

# Pre-populate Windows MSVC CRT + Windows SDK cache (x86_64 & aarch64) so CI jobs
# do not depend on live Microsoft download availability at build time.
RUN set -eux; \
    if [ "${PREFETCH_XWIN}" = "1" ]; then \
      mkdir -p "${XWIN_CACHE_DIR}"; \
      cargo install --locked xwin; \
      xwin --accept-license --cache-dir "${XWIN_CACHE_DIR}" --manifest-version "${XWIN_VERSION}" --arch x86_64,aarch64 download; \
      rm -rf "${CARGO_HOME}/registry/cache" "${CARGO_HOME}/registry/src"; \
    fi

WORKDIR /workspace
