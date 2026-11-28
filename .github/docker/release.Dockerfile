# syntax=docker/dockerfile:1.7

# Thin release runtime image that packages prebuilt Linux server binaries
# produced by release.yml's `build-linux-x64` job (Ubuntu 24.04, glibc 2.39).
# Debian Trixie (glibc 2.41) satisfies the glibc requirement while keeping the
# runtime environment aligned with the root source Dockerfile.
FROM debian:trixie-slim AS runtime

ARG TARGETOS
ARG TARGETARCH

LABEL org.opencontainers.image.source="https://github.com/Stravia-AI/StraviaPlatform" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 nonroot \
    && useradd --uid 65532 --gid nonroot --create-home --shell /usr/sbin/nologin nonroot \
    && install -d -o nonroot -g nonroot -m 0750 /data

COPY --chmod=0755 ${TARGETOS}/${TARGETARCH}/stravia-server /usr/local/bin/stravia-server

ENV HOME=/home/nonroot \
    STRAVIA_HOST=0.0.0.0 \
    STRAVIA_PORT=23471 \
    STRAVIA_DATA_DIR=/data

USER nonroot:nonroot
EXPOSE 23471

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["sh", "-c", "curl --fail --silent --output /dev/null --max-time 2 \"http://127.0.0.1:${STRAVIA_PORT}/healthz\""]

ENTRYPOINT ["/usr/local/bin/stravia-server"]
