#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  exit 0
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'orphan-volume\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  printf '[{"Name":"orphan-volume","Labels":{"decune.managed":"true","decune.workspace_id":"aaaaaaaaaaaa","decune.workspace":"/work/removed-repository"}}]\n'
  exit 0
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
