#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  count=0
  if [ -f "$DECUNE_FAKE_COUNT_FILE" ]; then
    count="$(cat "$DECUNE_FAKE_COUNT_FILE")"
  fi
  count=$((count + 1))
  printf '%s\n' "$count" >"$DECUNE_FAKE_COUNT_FILE"
  if [ "$count" -ge 2 ]; then
    printf '{"ID":"managed-container"}\n'
  fi
  exit 0
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
