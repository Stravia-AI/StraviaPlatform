#!/usr/bin/env bash
# Prepare patched Cargo crates and guarantee a complete multi-target Cargo registry cache.
# - `deps-cargo-v2` caches `target/patch` alongside `.task/checksum/prepare-crates`, so
#   `task prepare:crates` verifies source checksums and skips `cargo patch-crate --force`
#   in milliseconds on a clean cache hit while automatically re-patching if Taskfile.yml
#   or any patch input changes.
# - On cache miss, run `cargo fetch --locked` (without --target) so whichever job saves the
#   cache first populates crates for all targets instead of a partial single-target subset.
set -euo pipefail

CARGO_CACHE_HIT="${CARGO_CACHE_HIT:-false}"
TASK_BIN="${TASK_BIN:-task}"
CARGO_BIN="${CARGO_BIN:-cargo}"

"${TASK_BIN}" prepare:crates

if [[ "${CARGO_CACHE_HIT}" != "true" ]]; then
  "${CARGO_BIN}" fetch --locked
fi
