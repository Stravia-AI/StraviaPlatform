#!/usr/bin/env bash
set -euo pipefail

args=()
for arg in "$@"; do
  case "${arg}" in
    -lc++|-lstdc++)
      ;;
    *)
      args+=("${arg}")
      ;;
  esac
done

exec "${STRAVIA_REAL_ZIG:?STRAVIA_REAL_ZIG is required}" "${args[@]}"
