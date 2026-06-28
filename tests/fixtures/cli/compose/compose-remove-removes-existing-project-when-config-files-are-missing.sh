#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
project="decune-missing-config-$DECUNE_FAKE_WORKSPACE_ID"
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"decune.workspace_id=$DECUNE_FAKE_WORKSPACE_ID"*)
      if [ -f "$DECUNE_FAKE_REMOVED_MARKER" ]; then
        exit 0
      fi
      printf '{"ID":"compose-primary-id"}\n'
      exit 0
      ;;
    *"com.docker.compose.project=$project"*)
      printf '{"ID":"compose-primary-id"}\n'
      printf '{"ID":"compose-sidecar-id"}\n'
      exit 0
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  case " $* " in
    *compose-sidecar-id*)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/missing-db-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"com.docker.compose.project":"%s","com.docker.compose.service":"db"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project" "$project"
      exit 0
      ;;
    *)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  : > "$DECUNE_FAKE_REMOVED_MARKER"
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$project"*) printf 'missing_project_data\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$project"*) printf 'missing_project_default\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
