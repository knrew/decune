#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'invalid-container\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  shift 2
  case "$*" in
    "invalid-container")
      printf '[{"Id":"invalid-container","Name":"/invalid","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"../victim","decune.workspace":"/work/invalid"}},"State":{"Running":true}}]\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'invalid-volume\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  printf '[{"Name":"invalid-volume","Labels":{"decune.managed":"true","decune.workspace_id":"../victim"}}]\n'
  exit 0
fi

if [ "${1:-}" = stop ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
