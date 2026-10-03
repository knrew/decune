#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  for arg in "$@"; do
    case "$arg" in
      label=com.docker.compose.project=*)
        project="${arg#label=com.docker.compose.project=}"
        if [ "$project" = "${DECUNE_FAKE_NEW_VOLUME_PROJECT:-}" ] && [ -f "$DECUNE_FAKE_COMMAND_LOG" ]; then
          while IFS= read -r previous; do
            if [ "$previous" = "$project" ]; then
              printf '%s\n' "${project}_db"
              break
            fi
          done <"$DECUNE_FAKE_COMMAND_LOG"
        fi
        printf '%s\n' "$project" >>"$DECUNE_FAKE_COMMAND_LOG"
        ;;
    esac
  done
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
