#!/bin/sh
# Records each docker invocation, one line of arguments per call, and runs the real docker.
printf '%s\n' "$*" >>"$DECUNE_TEST_DOCKER_ARGV_LOG"
exec "$DECUNE_TEST_REAL_DOCKER" "$@"
