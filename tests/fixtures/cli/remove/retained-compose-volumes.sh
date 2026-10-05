#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  for project in $DECUNE_FAKE_PROJECTS; do
    if [ -f "$DECUNE_FAKE_DATA/$project-container" ]; then
      case "$*" in
        *"label=com.docker.compose.project=$project"* | *"label=decune.managed=true"*)
          printf '%s\n' "$project"
          ;;
      esac
    fi
  done
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  shift 2
  separator=""
  printf '['
  for project in "$@"; do
    printf '%s{"Id":"%s","Name":"/%s-app","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"123456abcdef","decune.workspace":"/work/app","com.docker.compose.project":"%s"}},"State":{"Running":false},"Mounts":[{"Type":"volume","Name":"%s_db"}]}' "$separator" "$project" "$project" "$project" "$project"
    separator=,
  done
  printf ']\n'
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  for project in $DECUNE_FAKE_PROJECTS; do
    case "$*" in
      *"label=com.docker.compose.project=$project"*)
        if [ -f "$DECUNE_FAKE_DATA/${project}_db" ]; then
          printf '%s\n' "${project}_db"
        fi
        ;;
    esac
  done
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  shift 2
  separator=""
  printf '['
  for volume in "$@"; do
    if [ -f "$DECUNE_FAKE_DATA/$volume" ]; then
      printf '%s{"Name":"%s","Labels":{"com.docker.compose.project":"%s"}}' "$separator" "$volume" "${volume%_db}"
      separator=,
    fi
  done
  printf ']\n'
  exit 0
fi
if [ "${1:-}" = stop ] || { [ "${1:-}" = network ] && [ "${2:-}" = ls ]; }; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  rm "$DECUNE_FAKE_DATA/${*: -1}-container"
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  volume="${*: -1}"
  if [ -f "$DECUNE_FAKE_DATA/outside" ]; then
    printf 'Error response from daemon: remove %s: volume is in use - [outside]\n' "$volume" >&2
    exit 1
  fi
  rm "$DECUNE_FAKE_DATA/$volume"
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
