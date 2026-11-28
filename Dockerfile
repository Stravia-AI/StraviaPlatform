# syntax=docker/dockerfile:1.7

ARG BUN_VERSION=1.4.2
ARG RUST_VERSION=1.99.1
FROM --platform=$BUILDPLATFORM oven/bun:${BUN_VERSION}-debian AS web-builder
WORKDIR /src

# Keep dependency installation cached until workspace manifests, the lockfile, or patches change.
COPY package.json bun.lock ./
COPY frontend/stravia-webui/package.json frontend/stravia-webui/package.json
COPY patches/ patches/
RUN --mount=type=cache,id=stravia-bun,target=/root/.bun/install/cache,sharing=locked \
    bun --bun run bun ci

COPY frontend/stravia-webui frontend/stravia-webui
RUN bun run build:web

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS rust-builder
ARG TARGETARCH
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        cmake libclang-dev python3 pkg-config \
        $([ "${TARGETARCH}" = "arm64" ] && echo "gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross") \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add wasm32-wasip2 \
    && ([ "${TARGETARCH}" != "arm64" ] || rustup target add aarch64-unknown-linux-gnu) \
    && cargo install patch-crate --version 0.1.13 --locked

COPY Cargo.toml Cargo.lock LICENSE-PLUGINS ./
COPY .cargo .cargo
COPY patches/ patches/
COPY backend backend
COPY --from=web-builder /src/frontend/stravia-webui/dist frontend/stravia-webui/dist
# Vendor plugins and the Gateway embed original SVGs; compiled WebUI assets cannot replace these inputs.
COPY --from=web-builder /src/frontend/stravia-webui/src/assets/icons frontend/stravia-webui/src/assets/icons
COPY --from=web-builder /src/frontend/stravia-webui/static/stravia-logo.svg frontend/stravia-webui/static/stravia-logo.svg

# Cache downloads and compiled dependencies without carrying build artifacts into the runtime image.
RUN --mount=type=cache,id=stravia-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=stravia-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=stravia-cargo-target-${TARGETARCH},target=/src/target,sharing=locked \
    set -eux; \
    cargo patch-crate --force; \
    cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors; \
    if [ "${TARGETARCH}" = "arm64" ]; then \
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
      export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc; \
      export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++; \
      export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar; \
      cargo build --locked --release -p stravia-server --target aarch64-unknown-linux-gnu; \
      install -Dm755 target/aarch64-unknown-linux-gnu/release/stravia-server /out/stravia-server; \
      aarch64-linux-gnu-strip /out/stravia-server; \
    else \
      cargo build --locked --release -p stravia-server; \
      install -Dm755 target/release/stravia-server /out/stravia-server; \
      strip /out/stravia-server; \
    fi; \
    install -d -m 0750 /out/data

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/Stravia-AI/StraviaPlatform" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 nonroot \
    && useradd --uid 65532 --gid nonroot --create-home --shell /usr/sbin/nologin nonroot

COPY --from=rust-builder /out/stravia-server /usr/local/bin/stravia-server
COPY --from=rust-builder --chown=nonroot:nonroot --chmod=0750 /out/data /data

ENV HOME=/home/nonroot \
    STRAVIA_HOST=0.0.0.0 \
    STRAVIA_PORT=23471 \
    STRAVIA_DATA_DIR=/data

USER nonroot:nonroot
EXPOSE 23471

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["sh", "-c", "curl --fail --silent --output /dev/null --max-time 2 \"http://127.0.0.1:${STRAVIA_PORT}/healthz\""]

ENTRYPOINT ["/usr/local/bin/stravia-server"]
