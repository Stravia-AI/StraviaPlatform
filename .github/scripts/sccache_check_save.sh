#!/usr/bin/env bash
# Print sccache statistics and decide whether the multi-GB sccache archive needs re-uploading.
# Skips redundant archive saves when the current lockfile/toolchain key prefix already matched
# and only a handful of crates missed the cache.
set -euo pipefail

SCCACHE_BIN="${SCCACHE_BIN:-sccache}"
CACHE_MATCHED_KEY="${CACHE_MATCHED_KEY:-}"
EXPECTED_KEY_PREFIX="${EXPECTED_KEY_PREFIX:-}"
MIN_MISSES="${SCCACHE_SAVE_MIN_MISSES:-15}"

stats=""
if command -v "${SCCACHE_BIN}" >/dev/null 2>&1; then
  stats="$("${SCCACHE_BIN}" --show-stats 2>&1 || true)"
  if [[ -n "${stats}" ]]; then
    printf '%s\n' "${stats}"
  fi
fi

parse_stat() {
  local label="$1"
  printf '%s\n' "${stats}" | awk -v label="${label}" '
    $0 ~ ("^" label "[[:space:]]+[0-9]+$") { print $NF; exit }
  '
}

hits="$(parse_stat 'Cache hits')"
misses="$(parse_stat 'Cache misses')"
hits="${hits:-0}"
misses="${misses:-0}"

should_save="false"
reason=""
if [[ -n "${SCCACHE_WEBDAV_ENDPOINT:-}" \
  || -n "${SCCACHE_ENDPOINT:-}" \
  || -n "${SCCACHE_BUCKET:-}" \
  || -n "${SCCACHE_REDIS:-}" \
  || -n "${SCCACHE_REDIS_ENDPOINT:-}" \
  || -n "${SCCACHE_GCS_BUCKET:-}" \
  || -n "${SCCACHE_AZURE_CONNECTION_STRING:-}" \
  || "${SCCACHE_GHA_ENABLED:-}" == "true" ]]; then
  should_save="false"
  reason="remote sccache backend is active; skipping actions/cache archive upload"
elif (( hits + misses == 0 )); then
  should_save="false"
  reason="no sccache compilations executed"
elif [[ -n "${EXPECTED_KEY_PREFIX}" && "${CACHE_MATCHED_KEY}" != "${EXPECTED_KEY_PREFIX}"* ]]; then
  should_save="true"
  reason="restored key '${CACHE_MATCHED_KEY:-<none>}' does not match prefix '${EXPECTED_KEY_PREFIX}' (hits=${hits}, misses=${misses})"
elif (( misses >= MIN_MISSES )); then
  should_save="true"
  reason="misses=${misses} >= threshold ${MIN_MISSES}"
else
  should_save="false"
  reason="matched current prefix '${EXPECTED_KEY_PREFIX}' with only ${misses} misses (< ${MIN_MISSES})"
fi

echo "sccache save decision: should_save=${should_save} (${reason})"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "should_save=${should_save}" >> "${GITHUB_OUTPUT}"
fi
