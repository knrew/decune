#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" down "*)
      echo 'service "removed-sidecar" has neither an image nor a build context specified: invalid compose project' >&2
      exit 1
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"com.docker.compose.project=$DECUNE_FAKE_PROJECT_NAME"*)
      printf 'compose-primary-id\n'
      printf 'compose-sidecar-id\n'
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-primary-id","Name":"/stale-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/stale-sidecar-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"com.docker.compose.project":"%s","com.docker.compose.service":"removed-sidecar"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$DECUNE_FAKE_PROJECT_NAME" "$DECUNE_FAKE_PROJECT_NAME"
  exit 0
fi
if [ "${1:-}" = stop ] || [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$DECUNE_FAKE_PROJECT_NAME"*) printf 'stale_project_data\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$DECUNE_FAKE_PROJECT_NAME"*) printf 'stale_project_default\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = rm ]; then
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
