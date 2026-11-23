#!/usr/bin/env bash
# Layer 2 Windows Desktop E2E on Linux x64 via ephemeral KVM/QEMU overlay VM.
#
# Preconditions (provisioned once on the Linux x64 KVM runner):
#   - /dev/kvm accessible to the runner user
#   - Base golden image (read-only qcow2) at $WINDOWS_BASE_IMAGE containing:
#       * Windows 11 / Windows Server 2022 Eval
#       * Microsoft Edge WebView2 Runtime
#       * OpenSSH Server enabled (key auth via $WINDOWS_SSH_KEY)
#       * Node.js 22+ and Bun 1.4.2 installed on PATH
#   - Cross-compiled `stravia-desktop.exe` (debug + desktop-e2e feature) and
#     `target/vendor-test-fixtures/` already built on the Linux host.
set -euo pipefail

WINDOWS_BASE_IMAGE="${WINDOWS_BASE_IMAGE:?WINDOWS_BASE_IMAGE (read-only qcow2 golden image) must be set}"
WINDOWS_SSH_KEY="${WINDOWS_SSH_KEY:?WINDOWS_SSH_KEY (private key path) must be set}"
WINDOWS_SSH_USER="${WINDOWS_SSH_USER:-stravia}"
WINDOWS_RAM="${WINDOWS_RAM:-8G}"
WINDOWS_CPUS="${WINDOWS_CPUS:-4}"
DESKTOP_EXE="${DESKTOP_EXE:-target/x86_64-pc-windows-msvc/debug/stravia-desktop.exe}"

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  echo "ERROR: /dev/kvm is not accessible; Layer-2 KVM Windows E2E requires hardware virtualization" >&2
  exit 1
fi
if [[ ! -f "${WINDOWS_BASE_IMAGE}" ]]; then
  echo "ERROR: Golden Windows image not found: ${WINDOWS_BASE_IMAGE}" >&2
  exit 1
fi
if [[ ! -f "${DESKTOP_EXE}" ]]; then
  echo "ERROR: Cross-compiled desktop E2E binary not found: ${DESKTOP_EXE}" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
OVERLAY_IMG="${TMP_DIR}/windows-ephemeral.qcow2"
SSH_PORT="$((22000 + RANDOM % 10000))"
QEMU_PID=""

cleanup() {
  if [[ -n "${QEMU_PID}" ]] && kill -0 "${QEMU_PID}" 2>/dev/null; then
    kill "${QEMU_PID}" 2>/dev/null || true
    wait "${QEMU_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

# Ephemeral CoW overlay: base golden image is never mutated.
qemu-img create -f qcow2 -F qcow2 -b "${WINDOWS_BASE_IMAGE}" "${OVERLAY_IMG}" >/dev/null

echo "==> Booting ephemeral Windows KVM guest (SSH forward localhost:${SSH_PORT})"
qemu-system-x86_64 \
  -enable-kvm \
  -machine q35,accel=kvm \
  -cpu host,hv_relaxed,hv_spinlocks=0x1fff,hv_vapic,hv_time \
  -smp "${WINDOWS_CPUS}" \
  -m "${WINDOWS_RAM}" \
  -drive "file=${OVERLAY_IMG},format=qcow2,if=virtio" \
  -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22" \
  -device virtio-net-pci,netdev=net0 \
  -display none \
  -serial "file:${TMP_DIR}/serial.log" &
QEMU_PID=$!

SSH_OPTS=(
  -i "${WINDOWS_SSH_KEY}"
  -p "${SSH_PORT}"
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o ConnectTimeout=5
  -o BatchMode=yes
)

echo "==> Waiting for Windows OpenSSH inside KVM guest..."
SSH_READY=0
for _ in $(seq 1 90); do
  if ! kill -0 "${QEMU_PID}" 2>/dev/null; then
    echo "ERROR: QEMU exited unexpectedly" >&2
    exit 1
  fi
  if ssh "${SSH_OPTS[@]}" "${WINDOWS_SSH_USER}@127.0.0.1" "cmd /c echo ready" >/dev/null 2>&1; then
    SSH_READY=1
    break
  fi
  sleep 2
done
if [[ "${SSH_READY}" != "1" ]]; then
  echo "ERROR: Timed out waiting for Windows VM SSH on port ${SSH_PORT}" >&2
  exit 1
fi

echo "==> Syncing repository & cross-compiled Windows artifacts into guest"
tar -czf "${TMP_DIR}/payload.tar.gz" \
  package.json bun.lock patches frontend/stravia-webui \
  "${DESKTOP_EXE}" \
  target/vendor-test-fixtures

scp -i "${WINDOWS_SSH_KEY}" -P "${SSH_PORT}" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  "${TMP_DIR}/payload.tar.gz" "${WINDOWS_SSH_USER}@127.0.0.1:C:/Users/${WINDOWS_SSH_USER}/payload.tar.gz"

echo "==> Running WebView2 + WDIO desktop smoke test inside Windows VM"
ssh "${SSH_OPTS[@]}" "${WINDOWS_SSH_USER}@127.0.0.1" powershell -NoProfile -NonInteractive -Command "\
  \$ErrorActionPreference = 'Stop'; \
  \$work = 'C:\\stravia-e2e'; \
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue \$work; \
  New-Item -ItemType Directory -Path \$work | Out-Null; \
  tar -xzf C:\\Users\\${WINDOWS_SSH_USER}\\payload.tar.gz -C \$work; \
  Set-Location \$work; \
  bun --bun run bun ci; \
  \$env:STRAVIA_DESKTOP_E2E_BINARY = Join-Path \$work '${DESKTOP_EXE//\//\\}'; \
  bun run test:e2e:desktop"
