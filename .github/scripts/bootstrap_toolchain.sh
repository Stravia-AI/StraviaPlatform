#!/usr/bin/env bash
# Idempotent toolchain bootstrap for unified GitHub Actions & Forgejo Actions Linux runners (x86_64 and aarch64).
# - No-ops quickly inside the prebuilt .github/docker/ci.Dockerfile image or after toolchain cache restore.
# - Supports persistent host cache volume (/ci-cache) AND standard actions/cache fallback.
# - Installs only the apt packages and toolchains requested by NEED_* flags.
set -euo pipefail

HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
  x86_64)
    SCCACHE_TRIPLE="x86_64-unknown-linux-musl"
    TASK_ARCH="amd64"
    UV_TRIPLE="x86_64-unknown-linux-gnu"
    ;;
  aarch64|arm64)
    SCCACHE_TRIPLE="aarch64-unknown-linux-musl"
    TASK_ARCH="arm64"
    UV_TRIPLE="aarch64-unknown-linux-gnu"
    ;;
  *)
    echo "Unsupported Linux host architecture: ${HOST_ARCH}" >&2
    exit 1
    ;;
esac

RUST_VERSION="${RUST_VERSION:-1.99.1}"
BUN_VERSION="${BUN_VERSION:-1.4.2}"
TASK_VERSION="${TASK_VERSION:-3.52.0}"
UV_VERSION="${UV_VERSION:-0.11.28}"
CARGO_XWIN_VERSION="${CARGO_XWIN_VERSION:-0.22.0}"
PATCH_CRATE_VERSION="${PATCH_CRATE_VERSION:-0.1.13}"
SCCACHE_VERSION="${SCCACHE_VERSION:-0.10.0}"
CI_CACHE_DIR="${CI_CACHE_DIR:-/ci-cache}"

NEED_RUST="${NEED_RUST:-1}"
NEED_BUN="${NEED_BUN:-1}"
NEED_TASK="${NEED_TASK:-1}"
NEED_UV="${NEED_UV:-0}"
NEED_MINISIGN="${NEED_MINISIGN:-0}"
NEED_XWIN="${NEED_XWIN:-0}"
NEED_LINUX_DESKTOP="${NEED_LINUX_DESKTOP:-0}"
NEED_LINUX_ARM64="${NEED_LINUX_ARM64:-0}"
NEED_LINUX_ARM64_DESKTOP="${NEED_LINUX_ARM64_DESKTOP:-0}"
NEED_WINE="${NEED_WINE:-0}"
NEED_MACOS_CROSS="${NEED_MACOS_CROSS:-0}"
RUST_TARGETS="${RUST_TARGETS:-wasm32-wasip2}"

OSXCROSS_ROOT="${OSXCROSS_ROOT:-/opt/osxcross}"
OSXCROSS_REV="${OSXCROSS_REV:-899260caf42d609293b12b88e8260d78b702cbf6}"
MACOS_SDK_VERSION="${MACOS_SDK_VERSION:-15.5}"
MACOS_SDK_URL="${MACOS_SDK_URL:-https://github.com/joseluisq/macosx-sdks/releases/download/15.5/MacOSX15.5.sdk.tar.xz}"
MACOS_SDK_SHA256="${MACOS_SDK_SHA256:-c15cf0f3f17d714d1aa5a642da8e118db53d79429eb015771ba816aa7c6c1cbd}"
APPLE_CODESIGN_VERSION="${APPLE_CODESIGN_VERSION:-0.29.0}"

HOST_CACHE="false"
if [[ -d "${CI_CACHE_DIR}" && -w "${CI_CACHE_DIR}" ]]; then
  HOST_CACHE="true"
  mkdir -p \
    "${CI_CACHE_DIR}/cargo" \
    "${CI_CACHE_DIR}/rustup" \
    "${CI_CACHE_DIR}/sccache" \
    "${CI_CACHE_DIR}/xwin" \
    "${CI_CACHE_DIR}/bun-install" \
    "${CI_CACHE_DIR}/bun" \
    "${CI_CACHE_DIR}/bin" \
    "${CI_CACHE_DIR}/uv" \
    "${CI_CACHE_DIR}/eslint" \
    "${CI_CACHE_DIR}/playwright"
  export CARGO_HOME="${CI_CACHE_DIR}/cargo"
  export RUSTUP_HOME="${CI_CACHE_DIR}/rustup"
  export SCCACHE_DIR="${CI_CACHE_DIR}/sccache"
  export XWIN_CACHE_DIR="${CI_CACHE_DIR}/xwin"
  export BUN_INSTALL="${CI_CACHE_DIR}/bun-install"
  export BUN_INSTALL_CACHE_DIR="${CI_CACHE_DIR}/bun"
  export LOCAL_BIN="${CI_CACHE_DIR}/bin"
  export UV_CACHE_DIR="${CI_CACHE_DIR}/uv"
  export ESLINT_CACHE_DIR="${CI_CACHE_DIR}/eslint"
  export PLAYWRIGHT_BROWSERS_PATH="${CI_CACHE_DIR}/playwright"
  if [[ ! -x "${OSXCROSS_ROOT}/target/bin/oa64-clang" ]]; then
    OSXCROSS_ROOT="${CI_CACHE_DIR}/osxcross"
  fi
  echo "Using persistent host cache volume at ${CI_CACHE_DIR}"
else
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export SCCACHE_DIR="${SCCACHE_DIR:-$HOME/.cache/sccache}"
  export XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$HOME/.cache/cargo-xwin}"
  export BUN_INSTALL="${BUN_INSTALL:-$HOME/.bun}"
  export BUN_INSTALL_CACHE_DIR="${BUN_INSTALL_CACHE_DIR:-$HOME/.bun/install/cache}"
  export LOCAL_BIN="${LOCAL_BIN:-$HOME/.local/bin}"
  export UV_CACHE_DIR="${UV_CACHE_DIR:-$HOME/.cache/uv}"
  export ESLINT_CACHE_DIR="${ESLINT_CACHE_DIR:-$HOME/.cache/eslint}"
  mkdir -p "${LOCAL_BIN}" "${ESLINT_CACHE_DIR}"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "host_cache=${HOST_CACHE}" >> "$GITHUB_OUTPUT"
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
  SUDO="sudo"
fi

export PATH="${CARGO_HOME}/bin:${BUN_INSTALL}/bin:${LOCAL_BIN}:$PATH"

apt_packages=()
add_pkg_if_missing() {
  local pkg="$1"
  if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "install ok installed"; then
    apt_packages+=("$pkg")
  fi
}

if command -v apt-get >/dev/null 2>&1; then
  for pkg in ca-certificates curl git python3; do
    add_pkg_if_missing "$pkg"
  done
  if [[ "$NEED_RUST" == "1" ]]; then
    for pkg in pkg-config libssl-dev cmake clang lld libclang-dev; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_LINUX_DESKTOP" == "1" ]]; then
    for pkg in libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev patchelf xdg-utils; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_XWIN" == "1" ]]; then
    for pkg in llvm lld nasm ninja-build nsis; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_MINISIGN" == "1" ]]; then
    add_pkg_if_missing "minisign"
  fi
  if [[ "$NEED_LINUX_ARM64" == "1" ]]; then
    for pkg in gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_LINUX_ARM64_DESKTOP" == "1" && "$HOST_ARCH" == "x86_64" ]]; then
    if ! dpkg --print-foreign-architectures 2>/dev/null | grep -qx "arm64"; then
      $SUDO dpkg --add-architecture arm64
    fi
    if [[ ! -f /etc/apt/sources.list.d/ubuntu-arm64-ports.sources ]]; then
      ubuntu_codename="$(. /etc/os-release && printf '%s' "${VERSION_CODENAME:-noble}")"
      if grep -Rq "mirrors.tuna.tsinghua.edu.cn" /etc/apt/sources.list /etc/apt/sources.list.d/ 2>/dev/null; then
        arm64_ports_uri="https://mirrors.tuna.tsinghua.edu.cn/ubuntu-ports/"
      else
        arm64_ports_uri="http://ports.ubuntu.com/ubuntu-ports/"
      fi
      # Pin existing x86_64 Ubuntu sources to amd64 so `apt-get update` does not 404 on arm64 indices.
      if [[ -f /etc/apt/sources.list.d/ubuntu.sources ]] && ! grep -q '^Architectures:' /etc/apt/sources.list.d/ubuntu.sources; then
        $SUDO sed -i '/^Types:/a Architectures: amd64' /etc/apt/sources.list.d/ubuntu.sources
      fi
      if [[ -f /etc/apt/sources.list ]]; then
        $SUDO sed -Ei 's|^deb (http[^ ]+)|deb [arch=amd64] \1|g' /etc/apt/sources.list
      fi
      $SUDO tee /etc/apt/sources.list.d/ubuntu-arm64-ports.sources >/dev/null <<EOF
Types: deb
URIs: ${arm64_ports_uri}
Suites: ${ubuntu_codename} ${ubuntu_codename}-updates ${ubuntu_codename}-security
Components: main universe restricted multiverse
Architectures: arm64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
    fi
    for pkg in libwebkit2gtk-4.1-dev:arm64 libgtk-3-dev:arm64 libayatana-appindicator3-dev:arm64 librsvg2-dev:arm64 libssl-dev:arm64 patchelf xdg-utils file; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_WINE" == "1" ]]; then
    for pkg in wine64 wine; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "$NEED_MACOS_CROSS" == "1" ]]; then
    for pkg in xz-utils bzip2 patch file libxml2-dev uuid-dev zlib1g-dev libbz2-dev liblzma-dev genisoimage hfsprogs; do
      add_pkg_if_missing "$pkg"
    done
  fi
  if [[ "${#apt_packages[@]}" -gt 0 ]]; then
    $SUDO rm -f /etc/apt/sources.list.d/google-chrome* || true
    $SUDO apt-get update -qq
    $SUDO apt-get install -y --no-install-recommends "${apt_packages[@]}"
  fi
fi

# Install sccache before any `cargo install` so even cold toolchain installs can benefit.
if [[ "$NEED_RUST" == "1" ]]; then
  if ! command -v sccache >/dev/null 2>&1; then
    mkdir -p "${LOCAL_BIN}"
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${SCCACHE_TRIPLE}.tar.gz" \
      | tar -xz --strip-components=1 -C "${LOCAL_BIN}" "sccache-v${SCCACHE_VERSION}-${SCCACHE_TRIPLE}/sccache" || true
  fi
fi

if [[ "$NEED_RUST" == "1" ]]; then
  current_rustc="$(rustc --version 2>/dev/null || true)"
  if command -v rustup >/dev/null 2>&1; then
    if ! rustup toolchain list --quiet 2>/dev/null | grep -q "^${RUST_VERSION}-"; then
      rustup toolchain install "${RUST_VERSION}" --profile minimal --component rustfmt,clippy
    fi
    rustup default "${RUST_VERSION}" >/dev/null 2>&1 || true
    installed_targets="$(rustup target list --toolchain "${RUST_VERSION}" --installed 2>/dev/null || true)"
    for target in ${RUST_TARGETS}; do
      if ! grep -qx "${target}" <<<"${installed_targets}"; then
        rustup target add --toolchain "${RUST_VERSION}" "${target}"
      fi
    done
  elif [[ "${current_rustc}" == *" ${RUST_VERSION} "* ]]; then
    echo "Using preinstalled rustc: ${current_rustc}"
  else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --default-toolchain "${RUST_VERSION}" --profile minimal --component rustfmt,clippy
    for target in ${RUST_TARGETS}; do
      rustup target add --toolchain "${RUST_VERSION}" "${target}"
    done
  fi
  if [[ "$(cargo patch-crate --version 2>/dev/null || true)" != "patch-crate ${PATCH_CRATE_VERSION}" ]]; then
    cargo install --locked --version "${PATCH_CRATE_VERSION}" patch-crate
  fi
  if [[ "$NEED_XWIN" == "1" ]] && ! command -v cargo-xwin >/dev/null 2>&1; then
    cargo install --locked --version "${CARGO_XWIN_VERSION}" cargo-xwin
  fi
  if [[ "$NEED_MACOS_CROSS" == "1" ]] && ! command -v rcodesign >/dev/null 2>&1; then
    mkdir -p "${LOCAL_BIN}"
    if ! curl -fsSL "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${APPLE_CODESIGN_VERSION}/apple-codesign-${APPLE_CODESIGN_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz --strip-components=1 -C "${LOCAL_BIN}" "apple-codesign-${APPLE_CODESIGN_VERSION}-x86_64-unknown-linux-musl/rcodesign"; then
      cargo install --locked --version "${APPLE_CODESIGN_VERSION}" apple-codesign
    fi
  fi
fi

if [[ "$NEED_MACOS_CROSS" == "1" ]]; then
  export PATH="${OSXCROSS_ROOT}/target/bin:${PATH}"
  if ! command -v oa64-clang >/dev/null 2>&1 || ! command -v o64-clang >/dev/null 2>&1; then
    $SUDO mkdir -p "${OSXCROSS_ROOT}"
    if [[ ! -d "${OSXCROSS_ROOT}/.git" ]]; then
      $SUDO git clone https://github.com/tpoechtrager/osxcross.git "${OSXCROSS_ROOT}"
    fi
    $SUDO git -C "${OSXCROSS_ROOT}" checkout "${OSXCROSS_REV}"
    sdk_tarball="${OSXCROSS_ROOT}/tarballs/MacOSX${MACOS_SDK_VERSION}.sdk.tar.xz"
    $SUDO curl -fsSL "${MACOS_SDK_URL}" -o "${sdk_tarball}"
    if [[ -n "${MACOS_SDK_SHA256}" ]]; then
      echo "${MACOS_SDK_SHA256}  ${sdk_tarball}" | sha256sum -c -
    fi
    (cd "${OSXCROSS_ROOT}" && $SUDO env CC=clang CXX=clang++ UNATTENDED=1 OSX_VERSION_MIN=11.0 ./build.sh)
  fi
  sdk_dir="$(find "${OSXCROSS_ROOT}/target/SDK" -maxdepth 1 -mindepth 1 -type d -name 'MacOSX*.sdk' | head -n 1)"
  arm_ar="$(find "${OSXCROSS_ROOT}/target/bin" -maxdepth 1 -name 'aarch64-apple-darwin[0-9]*-ar' | head -n 1)"
  x64_ar="$(find "${OSXCROSS_ROOT}/target/bin" -maxdepth 1 -name 'x86_64-apple-darwin[0-9]*-ar' | head -n 1)"
  if [[ -n "${sdk_dir}" ]]; then
    $SUDO ln -sfn "${sdk_dir}" "${OSXCROSS_ROOT}/target/SDK/current"
  fi
  if [[ -n "${arm_ar}" ]]; then
    $SUDO ln -sfn "${arm_ar}" "${OSXCROSS_ROOT}/target/bin/aarch64-apple-darwin-ar"
  fi
  if [[ -n "${x64_ar}" ]]; then
    $SUDO ln -sfn "${x64_ar}" "${OSXCROSS_ROOT}/target/bin/x86_64-apple-darwin-ar"
  fi
  if [[ -n "${GITHUB_PATH:-}" ]]; then
    printf '%s\n' "${OSXCROSS_ROOT}/target/bin" >> "$GITHUB_PATH"
  fi
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    cat <<EOF >> "$GITHUB_ENV"
OSXCROSS_TARGET_DIR=${OSXCROSS_ROOT}/target
OSXCROSS_SDK_DIR=${OSXCROSS_ROOT}/target/SDK/current
SDKROOT=${OSXCROSS_ROOT}/target/SDK/current
MACOSX_DEPLOYMENT_TARGET=11.0
CC_aarch64_apple_darwin=oa64-clang
CXX_aarch64_apple_darwin=oa64-clang++
AR_aarch64_apple_darwin=aarch64-apple-darwin-ar
CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=oa64-clang
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS=-C link-arg=-Wl,-headerpad_max_install_names
CC_x86_64_apple_darwin=o64-clang
CXX_x86_64_apple_darwin=o64-clang++
AR_x86_64_apple_darwin=x86_64-apple-darwin-ar
CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=o64-clang
CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS=-C link-arg=-Wl,-headerpad_max_install_names
EOF
  fi
fi

if [[ "$NEED_BUN" == "1" ]]; then
  if ! command -v bun >/dev/null 2>&1 || [[ "$(bun --version)" != "${BUN_VERSION}" ]]; then
    curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}"
  fi
fi

if [[ "$NEED_TASK" == "1" ]]; then
  if ! command -v task >/dev/null 2>&1; then
    mkdir -p "${LOCAL_BIN}"
    curl -fsSL "https://github.com/go-task/task/releases/download/v${TASK_VERSION}/task_linux_${TASK_ARCH}.tar.gz" \
      | tar -xz -C "${LOCAL_BIN}" task
  fi
fi

if [[ "$NEED_UV" == "1" ]]; then
  if ! command -v uv >/dev/null 2>&1; then
    mkdir -p "${LOCAL_BIN}"
    curl -fsSL "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-${UV_TRIPLE}.tar.gz" \
      | tar -xz --strip-components=1 -C "${LOCAL_BIN}" "uv-${UV_TRIPLE}/uv" "uv-${UV_TRIPLE}/uvx"
  fi
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
  printf '%s\n' "${CARGO_HOME}/bin" "${BUN_INSTALL}/bin" "${LOCAL_BIN}" >> "$GITHUB_PATH"
fi

if [[ -n "${GITHUB_ENV:-}" ]]; then
  cat <<EOF >> "$GITHUB_ENV"
CARGO_HOME=${CARGO_HOME}
RUSTUP_HOME=${RUSTUP_HOME}
SCCACHE_DIR=${SCCACHE_DIR}
SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE:-20G}
SCCACHE_IDLE_TIMEOUT=${SCCACHE_IDLE_TIMEOUT:-3600}
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0}
XWIN_CACHE_DIR=${XWIN_CACHE_DIR}
BUN_INSTALL=${BUN_INSTALL}
BUN_INSTALL_CACHE_DIR=${BUN_INSTALL_CACHE_DIR}
LOCAL_BIN=${LOCAL_BIN}
UV_CACHE_DIR=${UV_CACHE_DIR}
ESLINT_CACHE_DIR=${ESLINT_CACHE_DIR}
EOF
  if [[ -n "${PLAYWRIGHT_BROWSERS_PATH:-}" ]]; then
    echo "PLAYWRIGHT_BROWSERS_PATH=${PLAYWRIGHT_BROWSERS_PATH}" >> "$GITHUB_ENV"
  fi
  if command -v sccache >/dev/null 2>&1; then
    echo "RUSTC_WRAPPER=$(command -v sccache)" >> "$GITHUB_ENV"
  fi
  if [[ "${HOST_ARCH}" == "x86_64" ]] && command -v lld >/dev/null 2>&1 && command -v clang >/dev/null 2>&1; then
    echo "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang" >> "$GITHUB_ENV"
    echo "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C link-arg=-fuse-ld=lld" >> "$GITHUB_ENV"
  fi
fi
