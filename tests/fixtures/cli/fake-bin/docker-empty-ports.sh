#!/bin/sh
case "$*" in
  "ps --all --filter label=decune.managed=true --format {{.ID}}") exit 0 ;;
esac
echo "unexpected fake docker command: $*" >&2
exit 64
