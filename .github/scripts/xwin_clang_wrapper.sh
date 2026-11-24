#!/usr/bin/env bash
# Translates cargo-xwin's MSVC `/imsvc <dir>` include flags to GNU `-isystem <dir>`
# when `ring`'s build.rs overrides `CC` to GNU-driver `clang` for `aarch64-pc-windows-msvc`
# while inheriting `cargo-xwin`'s clang-cl `CFLAGS_aarch64_pc_windows_msvc`.
set -euo pipefail

REAL_CLANG="${STRAVIA_REAL_CLANG:?STRAVIA_REAL_CLANG must point to the real clang binary}"

case "$(basename "$0")" in
  clang-cl*)
    exec "${REAL_CLANG}" --driver-mode=cl "$@"
    ;;
esac

has_imsvc=0
for arg in "$@"; do
  case "$arg" in
    /imsvc|/imsvc*)
      has_imsvc=1
      break
      ;;
  esac
done

if [[ "${has_imsvc}" -eq 0 ]]; then
  exec "${REAL_CLANG}" "$@"
fi

args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    /imsvc)
      shift
      if [[ $# -gt 0 ]]; then
        args+=("-isystem" "$1")
        shift
      fi
      ;;
    /imsvc*)
      args+=("-isystem" "${1#/imsvc}")
      shift
      ;;
    *)
      args+=("$1")
      shift
      ;;
  esac
done

exec "${REAL_CLANG}" "${args[@]}"
