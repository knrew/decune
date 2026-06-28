#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","scale":2,"ports":[{"target":3000,"published":"3000","protocol":"tcp"}]}}}\n'
      exit 0
      ;;
  esac
fi
echo "unexpected fake docker command: $*" >&2
exit 91
