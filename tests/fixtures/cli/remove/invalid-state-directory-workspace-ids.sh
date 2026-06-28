#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      exit 0
      ;;
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      exit 0
      ;;
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

echo "unexpected fake docker command: $*" >&2
exit 91
