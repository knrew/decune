#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'managed-container\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"managed-container","Name":"/managed","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"%s"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID"
  exit 0
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
