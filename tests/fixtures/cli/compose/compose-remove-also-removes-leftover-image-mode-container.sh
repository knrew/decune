#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" down "*)
      printf 'compose down\n' >>"$DECUNE_FAKE_COMMAND_LOG"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  count=0
  if [ -f "$DECUNE_FAKE_PS_COUNT" ]; then
    count=$(cat "$DECUNE_FAKE_PS_COUNT")
  fi
  count=$((count + 1))
  printf '%s' "$count" >"$DECUNE_FAKE_PS_COUNT"
  if [ "$count" -eq 1 ]; then
    exit 0
  fi
  printf '{"ID":"old-image-id"}\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"old-image-id","Name":"/old-image","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"workspace","decune.config_hash":"old-hash","devcontainer.config_file":".devcontainer/devcontainer.json"}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = rm ]; then
  printf 'docker %s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
