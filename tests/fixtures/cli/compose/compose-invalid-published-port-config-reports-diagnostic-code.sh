#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      echo 'invalid IP address: 999.999.999.999' >&2
      exit 1
      ;;
  esac
fi
echo "unexpected fake docker command: $*" >&2
exit 91
