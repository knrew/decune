#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
