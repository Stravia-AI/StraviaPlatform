#!/usr/bin/env bash
# Layer 2 Windows Desktop E2E on Linux x64 via ephemeral KVM/QEMU overlay VM.
#
# Preconditions (provisioned via `nix build .#windows-e2e-image` or `.github/scripts/build_windows_kvm_image.sh`):
#   - /dev/kvm accessible to the runner user
#   - Read-only golden qcow2 at $WINDOWS_BASE_IMAGE containing:
#       * Windows 10 Enterprise LTSC 2021 Eval (24G trimmed + CompactOS + zstd)
#       * AutoLogon into interactive desktop Session 1 (1920x1080)
#       * WebView2 Standalone Runtime, OpenSSH Server (dynamic STRAVIA_SEED key sync),
#         Node.js 22, Bun 1.4.2, uv, standalone Python 3.12
#   - Cross-compiled `stravia-desktop.exe` (debug + desktop-e2e feature) and
#     `target/vendor-test-fixtures/` already built on the Linux host.
#
# Key environment options:
#   WINDOWS_BASE_IMAGE      Required path to read-only golden qcow2
#   WINDOWS_SSH_KEY         Optional private key path; omitted -> fresh ephemeral ed25519 key per run
#   WINDOWS_SUITES          Comma-separated suites (default: desktop,legacy,database,storage-failures)
#   WINDOWS_OFFLINE         1 = QEMU user-net restrict=on (requires pre-warmed host deps cache)
#   WINDOWS_DEPS_CACHE_DIR  Host dir for lockfile-keyed bun-cache/uv-cache tarballs
#                           (default: ${XDG_CACHE_HOME:-~/.cache}/stravia-win-vm/deps-cache)
#   WINDOWS_BUN_CACHE_TAR   Explicit override path for bun-cache tarball
#   WINDOWS_UV_CACHE_TAR    Explicit override path for uv-cache tarball
#   WINDOWS_SAVE_DEPS_CACHE 1 (default) = pull back warmed caches on miss to host deps cache dir
#   WINDOWS_WARM_CACHE_ONLY 1 = warm host bun-cache & uv-cache tarballs without running E2E suites
#   WINDOWS_RTC_BASE        Optional QEMU -rtc base=<value>; in WINDOWS_OFFLINE=1 defaults to
#                           sibling metadata.json buildDate if present (prevents Eval clock expiry)
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
WINDOWS_BASE_IMAGE="${WINDOWS_BASE_IMAGE:?WINDOWS_BASE_IMAGE (read-only qcow2 golden image) must be set}"
WINDOWS_SSH_KEY="${WINDOWS_SSH_KEY:-}"
WINDOWS_SSH_USER="${WINDOWS_SSH_USER:-stravia}"
WINDOWS_RAM="${WINDOWS_RAM:-8G}"
WINDOWS_CPUS="${WINDOWS_CPUS:-4}"
WINDOWS_SUITES="${WINDOWS_SUITES:-desktop,legacy,database,storage-failures}"
WINDOWS_OFFLINE="${WINDOWS_OFFLINE:-0}"
WINDOWS_SAVE_DEPS_CACHE="${WINDOWS_SAVE_DEPS_CACHE:-1}"
WINDOWS_WARM_CACHE_ONLY="${WINDOWS_WARM_CACHE_ONLY:-0}"
WINDOWS_DEPS_CACHE_DIR="${WINDOWS_DEPS_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME}/.cache}/stravia-win-vm/deps-cache}"
WINDOWS_BUN_CACHE_TAR="${WINDOWS_BUN_CACHE_TAR:-}"
WINDOWS_UV_CACHE_TAR="${WINDOWS_UV_CACHE_TAR:-}"
WINDOWS_RTC_BASE="${WINDOWS_RTC_BASE:-}"
WINDOWS_OVMF_CODE="${WINDOWS_OVMF_CODE:-}"
WINDOWS_OVMF_VARS="${WINDOWS_OVMF_VARS:-}"
WINDOWS_TPM="${WINDOWS_TPM:-0}"
DESKTOP_EXE="${DESKTOP_EXE:-target/x86_64-pc-windows-msvc/debug/stravia-desktop.exe}"
HARNESS_EXE="${HARNESS_EXE:-target/x86_64-pc-windows-msvc/debug/stravia-storage-test-harness.exe}"
QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
QEMU_IMG_BIN="${QEMU_IMG_BIN:-$(command -v qemu-img || true)}"
MKISOFS_BIN="${MKISOFS_BIN:-$(command -v genisoimage || command -v mkisofs || command -v xorriso || true)}"
PYTHON3_BIN="${PYTHON3_BIN:-$(command -v python3 || true)}"

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  echo "ERROR: /dev/kvm is not accessible; Layer-2 KVM Windows E2E requires hardware virtualization" >&2
  exit 1
fi
for pair in \
  "qemu-system-x86_64:${QEMU_BIN}" \
  "qemu-img:${QEMU_IMG_BIN}" \
  "genisoimage/mkisofs/xorriso:${MKISOFS_BIN}" \
  "python3:${PYTHON3_BIN}"; do
  name="${pair%%:*}"
  bin="${pair#*:}"
  if [[ -z "${bin}" || ! -x "${bin}" ]]; then
    echo "ERROR: Required host tool not found: ${name}" >&2
    exit 1
  fi
done
if [[ ! -f "${WINDOWS_BASE_IMAGE}" ]]; then
  echo "ERROR: Golden Windows image not found: ${WINDOWS_BASE_IMAGE}" >&2
  exit 1
fi
if [[ -n "${WINDOWS_SSH_KEY}" && ! -f "${WINDOWS_SSH_KEY}" ]]; then
  echo "ERROR: Windows SSH private key not found: ${WINDOWS_SSH_KEY}" >&2
  exit 1
fi
if [[ "${WINDOWS_WARM_CACHE_ONLY}" != "1" ]]; then
  if [[ ! -f "${REPO_ROOT}/${DESKTOP_EXE}" && ! -f "${DESKTOP_EXE}" ]]; then
    echo "ERROR: Cross-compiled desktop E2E binary not found: ${DESKTOP_EXE}" >&2
    exit 1
  fi
  if [[ ",${WINDOWS_SUITES}," == *",storage-failures,"* && ! -f "${REPO_ROOT}/${HARNESS_EXE}" && ! -f "${HARNESS_EXE}" ]]; then
    echo "ERROR: storage-failures suite requested but harness binary not found: ${HARNESS_EXE}" >&2
    echo "       Build it with: cargo xwin build --locked -p stravia-server --bin stravia-storage-test-harness --features stravia-server/test-harness --target x86_64-pc-windows-msvc" >&2
    exit 1
  fi
fi

BASE_IMAGE_ABS="$(realpath "${WINDOWS_BASE_IMAGE}")"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/stravia-win-kvm.XXXXXX")"
OVERLAY_IMG="${TMP_DIR}/windows-ephemeral.qcow2"
SEED_ISO="${TMP_DIR}/seed.iso"
SSH_PORT="$((22000 + RANDOM % 10000))"
QEMU_PID=""
SWTPM_PID=""

cleanup() {
  set +e
  if [[ -n "${QEMU_PID}" ]] && kill -0 "${QEMU_PID}" 2>/dev/null; then
    kill "${QEMU_PID}" 2>/dev/null || true
    wait "${QEMU_PID}" 2>/dev/null || true
  fi
  if [[ -n "${SWTPM_PID}" ]] && kill -0 "${SWTPM_PID}" 2>/dev/null; then
    kill "${SWTPM_PID}" 2>/dev/null || true
    wait "${SWTPM_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMP_DIR}" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Resolve or generate ephemeral SSH keypair and build STRAVIA_SEED ISO (~360 KiB).
# Works with both new seed-enabled golden images and legacy static-key images.
mkdir -p "${TMP_DIR}/seed"
if [[ -z "${WINDOWS_SSH_KEY}" ]]; then
  WINDOWS_SSH_KEY="${TMP_DIR}/id_ed25519"
  ssh-keygen -t ed25519 -N "" -C "stravia-win-kvm-ephemeral" -f "${WINDOWS_SSH_KEY}" >/dev/null
  chmod 0600 "${WINDOWS_SSH_KEY}"
  cp "${WINDOWS_SSH_KEY}.pub" "${TMP_DIR}/seed/authorized_keys"
else
  ssh-keygen -y -f "${WINDOWS_SSH_KEY}" > "${TMP_DIR}/seed/authorized_keys"
fi
if [[ "$(basename "${MKISOFS_BIN}")" == "xorriso" ]]; then
  "${MKISOFS_BIN}" -as mkisofs -J -R -V "STRAVIA_SEED" -o "${SEED_ISO}" "${TMP_DIR}/seed" >/dev/null 2>&1
else
  "${MKISOFS_BIN}" -J -R -V "STRAVIA_SEED" -o "${SEED_ISO}" "${TMP_DIR}/seed" >/dev/null 2>&1
fi

# Resolve lockfile-keyed host dependency cache tarballs (externalized from OS base image).
BUN_LOCK_KEY="$(sha256sum "${REPO_ROOT}/bun.lock" | cut -c1-16)"
UV_LOCK_KEY="$(sha256sum "${REPO_ROOT}/uv.lock" | cut -c1-16)"
BUN_CACHE_TAR="${WINDOWS_BUN_CACHE_TAR:-${WINDOWS_DEPS_CACHE_DIR}/bun-cache-${BUN_LOCK_KEY}.tar}"
UV_CACHE_TAR="${WINDOWS_UV_CACHE_TAR:-${WINDOWS_DEPS_CACHE_DIR}/uv-cache-${UV_LOCK_KEY}.tar}"
NEED_UV_CACHE=0
if [[ "${WINDOWS_WARM_CACHE_ONLY}" == "1" || ",${WINDOWS_SUITES}," == *",storage-failures,"* ]]; then
  NEED_UV_CACHE=1
fi

# In offline mode, default QEMU RTC to buildDate + 1 day if metadata.json exists alongside
# the read-only image, preventing Win10 Eval clock expiration on immutable Nix images while
# staying strictly after the image's sealing timestamp (and keeping clock=host for hv_time).
if [[ -z "${WINDOWS_RTC_BASE}" && "${WINDOWS_OFFLINE}" == "1" ]]; then
  SIBLING_META="$(dirname "${BASE_IMAGE_ABS}")/metadata.json"
  if [[ -f "${SIBLING_META}" ]]; then
    WINDOWS_RTC_BASE="$("${PYTHON3_BIN}" -c '
import datetime, json, sys
d = json.load(open(sys.argv[1]))
b = d.get("buildDate", "")
if b:
    dt = datetime.date.fromisoformat(b) + datetime.timedelta(days=1)
    print(dt.isoformat() + "T00:00:00")
' "${SIBLING_META}" 2>/dev/null || true)"
  fi
fi

# Ephemeral CoW overlay: read-only base golden image is never mutated.
"${QEMU_IMG_BIN}" create -f qcow2 -F qcow2 -b "${BASE_IMAGE_ABS}" "${OVERLAY_IMG}" >/dev/null

QEMU_ARGS=(
  -enable-kvm
  -machine q35,accel=kvm
  -cpu host,hv_relaxed,hv_spinlocks=0x1fff,hv_vapic,hv_time
  -smp "${WINDOWS_CPUS}"
  -m "${WINDOWS_RAM}"
  -drive "file=${OVERLAY_IMG},format=qcow2,if=virtio,cache=writeback"
  -drive "file=${SEED_ISO},media=cdrom,readonly=on"
  -device virtio-net-pci,netdev=net0
  -device VGA,vgamem_mb=32
  -display none
  -serial "file:${TMP_DIR}/serial.log"
)

if [[ -n "${WINDOWS_RTC_BASE}" && "${WINDOWS_RTC_BASE}" != "none" ]]; then
  if [[ "${WINDOWS_RTC_BASE}" == "utc" || "${WINDOWS_RTC_BASE}" == "localtime" || "${WINDOWS_RTC_BASE}" == *T* ]]; then
    QEMU_ARGS+=(-rtc "base=${WINDOWS_RTC_BASE},clock=host")
  else
    QEMU_ARGS+=(-rtc "base=${WINDOWS_RTC_BASE}T00:00:00,clock=host")
  fi
fi

if [[ "${WINDOWS_OFFLINE}" == "1" ]]; then
  QEMU_ARGS+=(-netdev "user,id=net0,restrict=on,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22")
else
  QEMU_ARGS+=(-netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22")
fi

# Optional UEFI (OVMF) support for Windows 11 / GPT images.
if [[ -n "${WINDOWS_OVMF_CODE}" ]]; then
  QEMU_ARGS+=(-drive "if=pflash,format=raw,readonly=on,file=${WINDOWS_OVMF_CODE}")
  if [[ -n "${WINDOWS_OVMF_VARS}" ]]; then
    cp "${WINDOWS_OVMF_VARS}" "${TMP_DIR}/OVMF_VARS.fd"
    QEMU_ARGS+=(-drive "if=pflash,format=raw,file=${TMP_DIR}/OVMF_VARS.fd")
  fi
fi

# Optional software TPM 2.0 support for Windows 11 images.
if [[ "${WINDOWS_TPM}" == "1" ]]; then
  SWTPM_BIN="${SWTPM_BIN:-$(command -v swtpm || true)}"
  if [[ -z "${SWTPM_BIN}" || ! -x "${SWTPM_BIN}" ]]; then
    echo "ERROR: WINDOWS_TPM=1 requires swtpm on PATH" >&2
    exit 1
  fi
  mkdir -p "${TMP_DIR}/tpm"
  "${SWTPM_BIN}" socket --tpmstate "dir=${TMP_DIR}/tpm" --ctrl "type=unixio,path=${TMP_DIR}/swtpm.sock" --tpm2 >/dev/null 2>&1 &
  SWTPM_PID=$!
  for _ in $(seq 1 20); do
    [[ -S "${TMP_DIR}/swtpm.sock" ]] && break
    sleep 0.2
  done
  QEMU_ARGS+=(
    -chardev "socket,id=chrtpm,path=${TMP_DIR}/swtpm.sock"
    -tpmdev emulator,id=tpm0,chardev=chrtpm
    -device tpm-tis,tpmdev=tpm0
  )
fi

echo "==> Booting ephemeral Windows KVM guest (SSH forward localhost:${SSH_PORT}, offline=${WINDOWS_OFFLINE}, suites=${WINDOWS_SUITES})"
"${QEMU_BIN}" "${QEMU_ARGS[@]}" &
QEMU_PID=$!

SSH_OPTS=(
  -F /dev/null
  -i "${WINDOWS_SSH_KEY}"
  -p "${SSH_PORT}"
  -o ControlMaster=no
  -o ControlPath=none
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ConnectTimeout=5
  -o BatchMode=yes
)

SCP_OPTS=(
  -F /dev/null
  -i "${WINDOWS_SSH_KEY}"
  -P "${SSH_PORT}"
  -o ControlMaster=no
  -o ControlPath=none
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ConnectTimeout=10
  -o BatchMode=yes
)

run_win_ps() {
  local script="\$ProgressPreference = 'SilentlyContinue'
$1"
  local encoded
  encoded="$("${PYTHON3_BIN}" -c 'import base64,sys; print(base64.b64encode(sys.argv[1].encode("utf-16le")).decode("ascii"))' "${script}")"
  ssh "${SSH_OPTS[@]}" "${WINDOWS_SSH_USER}@127.0.0.1" \
    "powershell -NoProfile -NonInteractive -EncodedCommand ${encoded}"
}

echo "==> Waiting for Windows OpenSSH (and STRAVIA_SEED key sync) inside KVM guest..."
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

echo "==> Waiting for interactive desktop Session 1 (explorer.exe)..."
SESSION1_READY=0
for _ in $(seq 1 60); do
  if run_win_ps 'if (Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ge 1 }) { exit 0 } else { exit 1 }' >/dev/null 2>&1; then
    SESSION1_READY=1
    break
  fi
  sleep 2
done
if [[ "${SESSION1_READY}" != "1" ]]; then
  echo "ERROR: Timed out waiting for interactive Session 1 desktop (AutoLogon/explorer.exe)" >&2
  exit 1
fi

# Upload host-side lockfile-keyed dependency caches if available.
BUN_CACHE_UPLOADED=0
UV_CACHE_UPLOADED=0
if [[ -f "${BUN_CACHE_TAR}" ]]; then
  BUN_CACHE_SIZE="$(du -sh "${BUN_CACHE_TAR}" | awk '{print $1}')"
  echo "==> Restoring external Bun cache (${BUN_CACHE_SIZE}, key=${BUN_LOCK_KEY})..."
  scp "${SCP_OPTS[@]}" "${BUN_CACHE_TAR}" "${WINDOWS_SSH_USER}@127.0.0.1:bun-cache.tar"
  run_win_ps "
\$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Path 'C:\tools\bun-cache' -Force | Out-Null
& tar.exe -xf \"C:\Users\\${WINDOWS_SSH_USER}\bun-cache.tar\" -C 'C:\tools\bun-cache'
\$rc = \$LASTEXITCODE
Remove-Item -Force -ErrorAction SilentlyContinue \"C:\Users\\${WINDOWS_SSH_USER}\bun-cache.tar\"
if (\$rc -ne 0) { throw \"Failed to extract bun-cache.tar (exit \$rc)\" }
"
  BUN_CACHE_UPLOADED=1
fi

if [[ "${NEED_UV_CACHE}" == "1" && -f "${UV_CACHE_TAR}" ]]; then
  UV_CACHE_SIZE="$(du -sh "${UV_CACHE_TAR}" | awk '{print $1}')"
  echo "==> Restoring external uv cache (${UV_CACHE_SIZE}, key=${UV_LOCK_KEY})..."
  scp "${SCP_OPTS[@]}" "${UV_CACHE_TAR}" "${WINDOWS_SSH_USER}@127.0.0.1:uv-cache.tar"
  run_win_ps "
\$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Path 'C:\tools\uv-cache' -Force | Out-Null
& tar.exe -xf \"C:\Users\\${WINDOWS_SSH_USER}\uv-cache.tar\" -C 'C:\tools\uv-cache'
\$rc = \$LASTEXITCODE
Remove-Item -Force -ErrorAction SilentlyContinue \"C:\Users\\${WINDOWS_SSH_USER}\uv-cache.tar\"
if (\$rc -ne 0) { throw \"Failed to extract uv-cache.tar (exit \$rc)\" }
"
  UV_CACHE_UPLOADED=1
fi

echo "==> Packing repository test harness & cross-compiled Windows artifacts..."
TAR_ITEMS=(
  package.json
  bun.lock
  patches
  pyproject.toml
  uv.lock
  frontend/stravia-webui
  tests/common
)
if [[ "${WINDOWS_WARM_CACHE_ONLY}" != "1" ]]; then
  TAR_ITEMS+=(
    "${DESKTOP_EXE}"
    target/vendor-test-fixtures
  )
  if [[ ",${WINDOWS_SUITES}," == *",storage-failures,"* ]]; then
    TAR_ITEMS+=(
      "${HARNESS_EXE}"
      tests/e2e
      tests/__init__.py
    )
  fi
fi

EXISTING_TAR_ITEMS=()
for item in "${TAR_ITEMS[@]}"; do
  if [[ -e "${REPO_ROOT}/${item}" ]]; then
    EXISTING_TAR_ITEMS+=("${item}")
  fi
done

tar -C "${REPO_ROOT}" \
  --exclude='frontend/stravia-webui/node_modules' \
  --exclude='frontend/stravia-webui/.svelte-kit' \
  --exclude='frontend/stravia-webui/dist' \
  --exclude='frontend/stravia-webui/dist-desktop-e2e' \
  --exclude='**/__pycache__' \
  --exclude='.venv' \
  -czf "${TMP_DIR}/payload.tar.gz" \
  "${EXISTING_TAR_ITEMS[@]}"

PAYLOAD_SIZE="$(du -sh "${TMP_DIR}/payload.tar.gz" | awk '{print $1}')"
echo "==> Uploading payload (${PAYLOAD_SIZE}) into guest..."
scp "${SCP_OPTS[@]}" \
  "${TMP_DIR}/payload.tar.gz" "${WINDOWS_SSH_USER}@127.0.0.1:payload.tar.gz"

DESKTOP_EXE_WIN="${DESKTOP_EXE//\//\\}"
HARNESS_EXE_WIN="${HARNESS_EXE//\//\\}"

cat > "${TMP_DIR}/run-session1.ps1" <<PSEOF
\$ErrorActionPreference = 'Stop'
\$ProgressPreference = 'SilentlyContinue'
\$logPath = 'C:\stravia-e2e\run.log'
\$exitPath = 'C:\stravia-e2e\run.exit'
\$bunDirtyPath = 'C:\stravia-e2e\bun-cache.dirty'
\$uvDirtyPath = 'C:\stravia-e2e\uv-cache.dirty'
Remove-Item -Force -ErrorAction SilentlyContinue \$logPath, \$exitPath, \$bunDirtyPath, \$uvDirtyPath

function Log-Line([string]\$msg) {
  Add-Content -Path \$logPath -Value \$msg -Encoding UTF8
}

function Invoke-Logged([string]\$Exe, [string[]]\$Arguments) {
  \$prevPref = \$ErrorActionPreference
  \$ErrorActionPreference = 'Continue'
  try {
    & \$Exe @Arguments 2>&1 | ForEach-Object {
      Add-Content -Path \$logPath -Value "\$_" -Encoding UTF8
    }
    return \$LASTEXITCODE
  } finally {
    \$ErrorActionPreference = \$prevPref
  }
}

\$exitCode = 0
try {
  \$env:Path = "C:\tools\node;C:\tools\bun;C:\tools\uv;C:\tools\python312;C:\tools\python312\Scripts;\$env:Path"
  \$env:CI = '1'
  \$env:NO_COLOR = '1'
  \$env:FORCE_COLOR = '0'
  \$env:BUN_INSTALL_CACHE_DIR = 'C:\tools\bun-cache'
  \$env:UV_CACHE_DIR = 'C:\tools\uv-cache'
  \$env:UV_PYTHON = '3.12'
  \$env:UV_PYTHON_PREFERENCE = 'only-system'
  \$env:UV_PYTHON_DOWNLOADS = 'never'
  New-Item -ItemType Directory -Path 'C:\tools\bun-cache', 'C:\tools\uv-cache' -Force | Out-Null
  Set-Location 'C:\stravia-e2e'

  \$sessionId = (Get-Process -Id \$PID).SessionId
  Log-Line "==> Session1 runner started (PID=\$PID, SessionId=\$sessionId, User=\$env:USERNAME)"
  if (\$sessionId -lt 1) {
    throw "Expected interactive Session >= 1, got Session \$sessionId"
  }

  \$hasBunCache = (Get-ChildItem 'C:\tools\bun-cache' -ErrorAction SilentlyContinue | Measure-Object).Count -gt 0
  if (\$hasBunCache) {
    Log-Line "==> Installing JS workspace dependencies from external Bun cache (--offline)..."
    \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('ci', '--offline')
    if (\$rc -ne 0) {
      if ('${WINDOWS_OFFLINE}' -eq '1') {
        throw "bun ci --offline failed in offline mode (exit \$rc). Warm the host cache with WINDOWS_OFFLINE=0 or WINDOWS_WARM_CACHE_ONLY=1."
      }
      Log-Line "==> External Bun cache miss; falling back to online bun ci..."
      \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('ci')
      if (\$rc -ne 0) { throw "bun ci failed (exit \$rc)" }
      Set-Content -Path \$bunDirtyPath -Value '1' -Encoding Ascii
    }
  } else {
    if ('${WINDOWS_OFFLINE}' -eq '1') {
      throw "No Bun cache found for bun.lock (${BUN_LOCK_KEY}) in WINDOWS_OFFLINE=1 mode. Run once with WINDOWS_OFFLINE=0 (or WINDOWS_WARM_CACHE_ONLY=1) to populate ${WINDOWS_DEPS_CACHE_DIR}."
    }
    Log-Line "==> External Bun cache absent; running online bun ci to populate cache..."
    \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('ci')
    if (\$rc -ne 0) { throw "bun ci failed (exit \$rc)" }
    Set-Content -Path \$bunDirtyPath -Value '1' -Encoding Ascii
  }

  if ('${WINDOWS_WARM_CACHE_ONLY}' -eq '1') {
    Log-Line "==> Warming Python test dependency cache via uv sync..."
    \$rc = Invoke-Logged 'C:\tools\uv\uv.exe' @('sync', '--python', '3.12', '--locked', '--no-install-project', '--only-group', 'test')
    if (\$rc -ne 0) { throw "uv sync failed while warming cache (exit \$rc)" }
    Set-Content -Path \$uvDirtyPath -Value '1' -Encoding Ascii
    Log-Line "==> Cache warm-only mode completed."
  } else {
    Log-Line "==> Generating .svelte-kit/tsconfig.json via svelte-kit sync..."
    \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('run', '--cwd', 'frontend/stravia-webui', 'svelte-kit', 'sync')
    if (\$rc -ne 0) { throw "svelte-kit sync failed (exit \$rc)" }

    \$env:STRAVIA_DESKTOP_E2E_BINARY = Join-Path 'C:\stravia-e2e' '${DESKTOP_EXE_WIN}'
    \$env:STRAVIA_STORAGE_HARNESS_BINARY = Join-Path 'C:\stravia-e2e' '${HARNESS_EXE_WIN}'
    \$suites = '${WINDOWS_SUITES}'.Split(',') | ForEach-Object { \$_.Trim() } | Where-Object { \$_ -ne '' }

    foreach (\$suite in \$suites) {
      Log-Line ""
      Log-Line "================================================================"
      Log-Line "==> Running Windows Desktop E2E suite: \$suite"
      Log-Line "================================================================"
      switch (\$suite) {
        'desktop' {
          Remove-Item Env:STRAVIA_DESKTOP_E2E_STARTUP_FAILURE -ErrorAction SilentlyContinue
          \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('tests/common/run-in-tempdir.ts', 'bun', 'run', 'test:e2e:desktop')
          if (\$rc -ne 0) { throw "Suite 'desktop' failed with exit code \$rc" }
        }
        'legacy' {
          \$env:STRAVIA_DESKTOP_E2E_STARTUP_FAILURE = 'legacy'
          \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('tests/common/run-in-tempdir.ts', 'bun', 'run', 'test:e2e:desktop')
          if (\$rc -ne 0) { throw "Suite 'legacy' failed with exit code \$rc" }
        }
        'database' {
          \$env:STRAVIA_DESKTOP_E2E_STARTUP_FAILURE = 'database'
          \$rc = Invoke-Logged 'C:\tools\bun\bun.exe' @('tests/common/run-in-tempdir.ts', 'bun', 'run', 'test:e2e:desktop')
          if (\$rc -ne 0) { throw "Suite 'database' failed with exit code \$rc" }
        }
        'storage-failures' {
          Remove-Item Env:STRAVIA_DESKTOP_E2E_STARTUP_FAILURE -ErrorAction SilentlyContinue
          \$env:STRAVIA_RUN_NATIVE_STORAGE_TESTS = '1'
          \$hasUvCache = (Get-ChildItem 'C:\tools\uv-cache' -ErrorAction SilentlyContinue | Measure-Object).Count -gt 0
          if ('${WINDOWS_OFFLINE}' -eq '1' -and -not \$hasUvCache) {
            throw "No uv cache found for uv.lock (${UV_LOCK_KEY}) in WINDOWS_OFFLINE=1 mode. Run once with WINDOWS_OFFLINE=0 (or WINDOWS_WARM_CACHE_ONLY=1) to populate ${WINDOWS_DEPS_CACHE_DIR}."
          }
          \$syncArgs = @('sync', '--python', '3.12', '--locked', '--no-install-project', '--only-group', 'test')
          if (\$hasUvCache) {
            \$syncRc = Invoke-Logged 'C:\tools\uv\uv.exe' (\$syncArgs + @('--offline'))
            if (\$syncRc -ne 0) {
              if ('${WINDOWS_OFFLINE}' -eq '1') {
                throw "uv sync --offline failed in offline mode (exit \$syncRc)"
              }
              Log-Line "==> External uv cache miss; falling back to online uv sync..."
              \$syncRc = Invoke-Logged 'C:\tools\uv\uv.exe' \$syncArgs
              if (\$syncRc -ne 0) { throw "uv sync failed (exit \$syncRc)" }
              Set-Content -Path \$uvDirtyPath -Value '1' -Encoding Ascii
            }
          } else {
            \$syncRc = Invoke-Logged 'C:\tools\uv\uv.exe' \$syncArgs
            if (\$syncRc -ne 0) { throw "uv sync failed (exit \$syncRc)" }
            Set-Content -Path \$uvDirtyPath -Value '1' -Encoding Ascii
          }
          \$rc = Invoke-Logged 'C:\tools\uv\uv.exe' @('run', '--python', '3.12', '--locked', '--no-sync', '--group', 'test', 'python', '-m', 'pytest', 'tests/e2e/storage/test_storage_failures.py', '-k', 'desktop', '-q', '--durations=20')
          if (\$rc -ne 0) { throw "Suite 'storage-failures' failed with exit code \$rc" }
        }
        default {
          throw "Unknown Windows desktop E2E suite: \$suite"
        }
      }
      Log-Line "==> Suite '\$suite' PASSED"
    }
  }
} catch {
  Log-Line ("ERROR: " + \$_.Exception.Message)
  \$exitCode = 1
} finally {
  Set-Content -Path \$exitPath -Value \$exitCode -Encoding Ascii
}
PSEOF

echo "==> Uploading Session 1 runner & extracting payload into C:\\stravia-e2e..."
scp "${SCP_OPTS[@]}" \
  "${TMP_DIR}/run-session1.ps1" "${WINDOWS_SSH_USER}@127.0.0.1:run-session1.ps1"

run_win_ps "
\$ErrorActionPreference = 'Continue'
\$work = 'C:\stravia-e2e'
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue \$work
New-Item -ItemType Directory -Path \$work -Force | Out-Null
& tar.exe -xzf \"C:\Users\\${WINDOWS_SSH_USER}\payload.tar.gz\" -C \$work
if (\$LASTEXITCODE -ne 0) { throw \"tar extraction failed with exit code \$LASTEXITCODE\" }
Move-Item -Force \"C:\Users\\${WINDOWS_SSH_USER}\run-session1.ps1\" (Join-Path \$work 'run-session1.ps1')
"

echo "==> Launching E2E suites in interactive Session 1 via Scheduled Task (/IT)..."
set +e
run_win_ps "
\$ErrorActionPreference = 'Continue'
\$taskName = 'StraviaE2eSession1'
& schtasks.exe /Delete /TN \$taskName /F 2>&1 | Out-Null
& schtasks.exe /Create /TN \$taskName /SC ONCE /ST 00:00 /RU '${WINDOWS_SSH_USER}' /IT /RL HIGHEST /F /TR 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\stravia-e2e\run-session1.ps1' 2>&1 | Out-Null
if (\$LASTEXITCODE -ne 0) { throw \"schtasks /Create failed (exit \$LASTEXITCODE)\" }
& schtasks.exe /Run /TN \$taskName 2>&1 | Out-Null
if (\$LASTEXITCODE -ne 0) { throw \"schtasks /Run failed (exit \$LASTEXITCODE)\" }
\$logPath = 'C:\stravia-e2e\run.log'
\$exitPath = 'C:\stravia-e2e\run.exit'
\$seen = 0
while (\$true) {
  if (Test-Path \$logPath) {
    \$lines = @(Get-Content -Path \$logPath -Encoding UTF8 -ErrorAction SilentlyContinue)
    if (\$lines.Count -gt \$seen) {
      \$lines[\$seen..(\$lines.Count - 1)] | ForEach-Object { Write-Output \$_ }
      \$seen = \$lines.Count
    }
  }
  if (Test-Path \$exitPath) { break }
  Start-Sleep -Milliseconds 500
}
if (Test-Path \$logPath) {
  \$lines = @(Get-Content -Path \$logPath -Encoding UTF8 -ErrorAction SilentlyContinue)
  if (\$lines.Count -gt \$seen) {
    \$lines[\$seen..(\$lines.Count - 1)] | ForEach-Object { Write-Output \$_ }
  }
}
& schtasks.exe /Delete /TN \$taskName /F 2>&1 | Out-Null
\$code = [int](Get-Content -Path \$exitPath -Raw).Trim()
exit \$code
"
SUITE_EXIT=$?
set -e

# Persist newly populated external caches back to host (atomic mv) even if a later suite fails,
# so subsequent runs can reuse them offline without re-downloading.
save_guest_cache() {
  local name="$1"
  local dirty_marker="$2"
  local guest_dir="$3"
  local host_dest="$4"
  if [[ "${WINDOWS_SAVE_DEPS_CACHE}" != "1" || "${WINDOWS_OFFLINE}" == "1" ]]; then
    return 0
  fi
  if ! run_win_ps "if (Test-Path '${dirty_marker}') { exit 0 } else { exit 1 }" >/dev/null 2>&1; then
    return 0
  fi
  echo "==> Saving warmed ${name} cache to host (${host_dest})..."
  mkdir -p "$(dirname "${host_dest}")"
  if run_win_ps "
\$ErrorActionPreference = 'Continue'
\$archive = \"C:\Users\\${WINDOWS_SSH_USER}\\${name}.tar\"
Remove-Item -Force -ErrorAction SilentlyContinue \$archive
& tar.exe -cf \$archive -C '${guest_dir}' .
exit \$LASTEXITCODE
" >/dev/null 2>&1; then
    local tmp_host="${host_dest}.tmp.$$"
    if scp "${SCP_OPTS[@]}" "${WINDOWS_SSH_USER}@127.0.0.1:${name}.tar" "${tmp_host}" >/dev/null 2>&1; then
      mv -f "${tmp_host}" "${host_dest}"
      echo "==> Saved ${name} cache ($(du -sh "${host_dest}" | awk '{print $1}'))"
    else
      rm -f "${tmp_host}"
      echo "WARNING: Failed to download ${name}.tar from guest" >&2
    fi
  else
    echo "WARNING: Failed to archive ${guest_dir} inside guest" >&2
  fi
}

if [[ "${BUN_CACHE_UPLOADED}" == "0" || "${WINDOWS_SAVE_DEPS_CACHE}" == "1" ]]; then
  save_guest_cache "bun-cache" 'C:\stravia-e2e\bun-cache.dirty' 'C:\tools\bun-cache' "${BUN_CACHE_TAR}"
fi
if [[ "${NEED_UV_CACHE}" == "1" ]]; then
  save_guest_cache "uv-cache" 'C:\stravia-e2e\uv-cache.dirty' 'C:\tools\uv-cache' "${UV_CACHE_TAR}"
fi

exit "${SUITE_EXIT}"
