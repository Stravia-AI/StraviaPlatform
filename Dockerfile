# syntax=docker/dockerfile:1.7

ARG BUN_VERSION=1.4.0
ARG RUST_VERSION=1.97.1

FROM oven/bun:${BUN_VERSION}-debian AS web-builder
WORKDIR /src

# Keep dependency installation cached until a workspace manifest or lockfile changes.
COPY package.json bun.lock ./
COPY frontend/stravia-webui/package.json frontend/stravia-webui/package.json
RUN --mount=type=cache,id=stravia-bun,target=/root/.bun/install/cache,sharing=locked \
    bun ci

COPY frontend/stravia-webui frontend/stravia-webui
RUN bun run build:web

FROM rust:${RUST_VERSION}-bookworm AS rust-builder
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY backend backend
COPY --from=web-builder /src/frontend/stravia-webui/dist frontend/stravia-webui/dist

# Cache downloads and compiled dependencies without carrying build artifacts into the runtime image.
RUN --mount=type=cache,id=stravia-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=stravia-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=stravia-cargo-target,target=/src/target,sharing=locked \
    cargo build --locked --release -p stravia-server \
    && install -Dm755 target/release/stravia-server /out/stravia-server \
    && strip /out/stravia-server \
    && install -d -m 0750 /out/data

FROM gcr.io/distroless/cc-debian12:debug-nonroot AS runtime

LABEL org.opencontainers.image.source="https://github.com/Stravia-AI/StraviaPlatform" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

COPY --from=rust-builder /out/stravia-server /usr/local/bin/stravia-server
COPY --from=rust-builder --chown=nonroot:nonroot --chmod=0750 /out/data /data

ENV HOME=/home/nonroot \
    STRAVIA_HOST=0.0.0.0 \
    STRAVIA_PORT=23471 \
    STRAVIA_DATA_DIR=/data

USER nonroot:nonroot
EXPOSE 23471

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/busybox/sh", "-c", "/busybox/wget --spider --quiet -T 2 \"http://127.0.0.1:${STRAVIA_PORT}/healthz\""]

ENTRYPOINT ["/usr/local/bin/stravia-server"]
