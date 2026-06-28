#!/bin/sh
if [ "$1" = auth ] && [ "$2" = token ]; then
  printf 'github-test-secret\n'
  exit 0
fi
exit 91
