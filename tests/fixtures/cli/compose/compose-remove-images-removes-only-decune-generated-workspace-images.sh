#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" down "*)
      case " $* " in
        *" --rmi "*) echo "compose down must not remove user images" >&2; exit 44 ;;
      esac
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = ls ]; then
  reference=
  for argument in "$@"; do
    reference=$argument
  done
  if [ "$reference" != "$DECUNE_FAKE_IMAGE_REPOSITORY:*" ]; then
    echo "unexpected image list reference: $reference" >&2
    exit 45
  fi
  printf '{"Repository":"%s","Tag":"final-hash"}\n' "$DECUNE_FAKE_IMAGE_REPOSITORY"
  printf '{"Repository":"example/sidecar","Tag":"dev"}\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  if [ "${3:-}" = "--no-prune" ] && [ "${4:-}" = "--force" ] && [ "${5:-}" = "$DECUNE_FAKE_IMAGE_REPOSITORY:final-hash" ] && [ "$#" -eq 5 ]; then
    exit 0
  fi
  echo "unexpected image removal: $*" >&2
  exit 46
fi
echo "unexpected fake docker command: $*" >&2
exit 91
