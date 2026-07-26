#!/bin/sh
set -eu
case " $* " in
  *decune-config-secret* | *devcontainer-secret*)
    echo "secret value was exposed in Docker CLI arguments" >&2
    exit 92
    ;;
esac
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      test "${DECUNE_CONTAINER_ENV_NPM_TOKEN:-}" = "decune-config-secret"
      test "${DECUNE_CONTAINER_ENV_DEVCONTAINER_TOKEN:-}" = "devcontainer-secret"
      previous=
      generated_override=
      for argument in "$@"; do
        if [ "$previous" = "-f" ]; then
          case "$argument" in
            *compose.override.yaml) generated_override=$argument ;;
          esac
        fi
        previous=$argument
      done
      test -n "$generated_override"
      cat "$generated_override" >"$DECUNE_FAKE_OVERRIDE_LOG"
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
