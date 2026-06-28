#!/bin/sh
if [ "$1" = auth ] && [ "$2" = token ]; then
  cat "$DECUNE_TEST_GH_TOKEN_FILE"
  exit 0
fi
exit 91
