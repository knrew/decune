#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","container_name":"fixed-app"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      echo "compose up should not run after clone isolation preflight failure" >&2
      exit 92
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  printf '{"ID":"other-container-id"}\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"other-container-id","Name":"/fixed-app","Config":{"Env":[],"Labels":{"com.docker.compose.project":"other-project"}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
