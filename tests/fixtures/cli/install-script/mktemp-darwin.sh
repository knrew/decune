#!/bin/sh
if [ "$#" -ne 2 ] || [ "$1" != "-d" ]; then
  echo "mktemp requires -d and a template" >&2
  exit 64
fi
case "$2" in
  */decune.XXXXXXXXXX) ;;
  *)
    echo "unexpected mktemp template: $2" >&2
    exit 64
    ;;
esac
printf '%s\n' "$1" "$2" >"$DECUNE_TEST_MKTEMP_ARGS"
dir="${2%XXXXXXXXXX}darwin"
mkdir -p "$dir"
printf '%s\n' "$dir"
