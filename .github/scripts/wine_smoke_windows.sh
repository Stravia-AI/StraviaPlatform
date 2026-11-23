#!/usr/bin/env bash
# Layer 1 Windows verification on Linux x64 via Wine (no Windows host required).
# Exercises:
#   1. Cross-compiled Windows PE `stravia-server.exe` startup + /healthz HTTP response + clean shutdown.
#   2. Cross-compiled Windows PE `stravia-desktop.exe --smoke-test-runtime` (real Wasmtime/SQLite runtime without WebView2).
#   3. Optional Windows Rust unit test binaries via Wine.
set -euo pipefail

WINE_BIN="${WINE_BIN:-$(command -v wine64 || command -v wine || true)}"
if [[ -z "${WINE_BIN}" ]]; then
  echo "ERROR: wine64/wine is required for Layer-1 Windows smoke testing" >&2
  exit 1
fi

SERVER_EXE="${1:-target/x86_64-pc-windows-msvc/debug/stravia-server.exe}"
DESKTOP_EXE="${2:-}"

if [[ ! -f "${SERVER_EXE}" ]]; then
  echo "ERROR: Windows server binary not found: ${SERVER_EXE}" >&2
  exit 1
fi

TMP_ROOT="$(mktemp -d)"
export WINEPREFIX="${TMP_ROOT}/wineprefix"
export WINEDEBUG="${WINEDEBUG:--all}"
export XDG_RUNTIME_DIR="${TMP_ROOT}/xdg"
mkdir -p "${WINEPREFIX}" "${XDG_RUNTIME_DIR}" "${TMP_ROOT}/data"

SERVER_PID=""
cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  wineserver -k 2>/dev/null || true
  rm -rf "${TMP_ROOT}"
}
trap cleanup EXIT

PORT="${STRAVIA_WINE_TEST_PORT:-$((24000 + RANDOM % 10000))}"
echo "==> Starting Windows binary under Wine: ${SERVER_EXE} (port ${PORT})"
STRAVIA_HOST="127.0.0.1" \
STRAVIA_PORT="${PORT}" \
STRAVIA_DATA_DIR="Z:${TMP_ROOT}/data" \
  "${WINE_BIN}" "${SERVER_EXE}" >"${TMP_ROOT}/server.log" 2>&1 &
SERVER_PID=$!

READY=0
for _ in $(seq 1 45); do
  if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    echo "ERROR: stravia-server.exe exited prematurely under Wine:" >&2
    cat "${TMP_ROOT}/server.log" >&2
    exit 1
  fi
  if curl --fail --silent --max-time 2 "http://127.0.0.1:${PORT}/healthz" >/dev/null 2>&1; then
    READY=1
    break
  fi
  sleep 1
done

if [[ "${READY}" != "1" ]]; then
  echo "ERROR: stravia-server.exe did not become healthy on port ${PORT} within 45s" >&2
  cat "${TMP_ROOT}/server.log" >&2
  exit 1
fi

echo "==> stravia-server.exe /healthz responded OK under Wine"
kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""

if [[ -n "${DESKTOP_EXE}" ]]; then
  if [[ ! -f "${DESKTOP_EXE}" ]]; then
    echo "ERROR: Windows desktop binary not found: ${DESKTOP_EXE}" >&2
    exit 1
  fi
  echo "==> Running Windows desktop Wasm runtime smoke test under Wine: ${DESKTOP_EXE}"
  timeout 180 "${WINE_BIN}" "${DESKTOP_EXE}" --smoke-test-runtime
  echo "==> stravia-desktop.exe --smoke-test-runtime passed under Wine"
fi
