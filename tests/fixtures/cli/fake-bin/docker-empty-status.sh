#!/bin/sh
case "$*" in
  "ps --all --filter label=decune.managed=true --format json") exit 0 ;;
  "ps --all --filter label=decune.managed=true --filter label=decune.workspace_id="*" --format json") exit 0 ;;
  "volume ls --filter label=decune.managed=true --format {{.Name}}") exit 0 ;;
  "volume ls --filter label=decune.managed=true --filter label=decune.workspace_id="*" --format {{.Name}}") exit 0 ;;
esac
echo "unexpected fake docker command: $*" >&2
exit 64
