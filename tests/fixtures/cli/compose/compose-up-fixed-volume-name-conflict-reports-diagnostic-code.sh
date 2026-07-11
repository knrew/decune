#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","volumes":["cache:/cache"]}},"volumes":{"cache":{"name":"fixed-cache"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      echo "compose up should not run after clone isolation preflight failure" >&2
      exit 92
      ;;
  esac
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ] && [ "${3:-}" = -- ] && [ "${4:-}" = fixed-cache ]; then
  printf '[{"Name":"fixed-cache","Labels":{"com.docker.compose.project":"other-project"}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
