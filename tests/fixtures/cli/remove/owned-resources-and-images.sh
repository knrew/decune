#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'standalone-id\n'
      printf 'compose-primary-id\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=compose-owned"*)
      printf 'compose-primary-id\n'
      printf 'compose-sidecar-id\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  shift 2
  case "$*" in
    "standalone-id compose-primary-id")
      printf '[{"Id":"standalone-id","Name":"/standalone","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"aaaaaaaaaaaa","decune.workspace":"/work/standalone-one"}},"State":{"Running":true}},{"Id":"compose-primary-id","Name":"/compose-owned-app-1","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"bbbbbbbbbbbb","decune.workspace":"/work/compose-one","com.docker.compose.project":"compose-owned","com.docker.compose.service":"app"}},"State":{"Running":true}}]\n'
      exit 0
      ;;
    "compose-primary-id compose-sidecar-id")
      printf '[{"Id":"compose-primary-id","Name":"/compose-owned-app-1","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"bbbbbbbbbbbb","com.docker.compose.project":"compose-owned"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/compose-owned-db-1","Config":{"Labels":{"com.docker.compose.project":"compose-owned","com.docker.compose.service":"db"}},"State":{"Running":false}}]\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'standalone-volume\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=compose-owned"*)
      printf 'compose-volume\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  printf '[{"Name":"standalone-volume","Labels":{"decune.managed":"true","decune.workspace_id":"aaaaaaaaaaaa"}}]\n'
  exit 0
fi

if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=com.docker.compose.project=compose-owned"*)
      printf 'compose-network\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = image ] && [ "${2:-}" = ls ]; then
  reference="${*: -1}"
  case "$reference" in
    decune/standalone-one-aaaaaaaaaaaa:*)
      printf '{"Repository":"decune/standalone-one-aaaaaaaaaaaa","Tag":"hash1"}\n'
      exit 0
      ;;
    decune/compose-one-bbbbbbbbbbbb:*)
      printf '{"Repository":"decune/compose-one-bbbbbbbbbbbb","Tag":"hash2"}\n'
      exit 0
      ;;
    decune/state-workspace-123456abcdef:*)
      printf '{"Repository":"decune/state-workspace-123456abcdef","Tag":"statehash"}\n'
      exit 0
      ;;
  esac
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
if [ "${1:-}" = network ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
