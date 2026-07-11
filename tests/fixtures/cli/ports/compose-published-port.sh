#!/bin/sh
set -eu
project="decune-test-$DECUNE_FAKE_WORKSPACE_ID"
case "$*" in
  *"ps --all"*"label=decune.managed=true"*"label=decune.workspace_id=$DECUNE_FAKE_WORKSPACE_ID"*"--format {{.ID}}"*)
    exit 0
    ;;
  *"ps --all"*"label=com.docker.compose.project=$project"*"--format {{.ID}}"*)
    printf 'compose-web-id\n'
    exit 0
    ;;
  "container inspect compose-web-id")
    printf '[{"Id":"compose-web-id","Name":"/compose-web-1","Config":{"Labels":{"com.docker.compose.project":"%s","com.docker.compose.service":"web"}},"State":{"Running":true},"NetworkSettings":{"Ports":{"3000/tcp":[{"HostIp":"0.0.0.0","HostPort":"%s"},{"HostIp":"::","HostPort":"%s"}]}}}]\n' "$project" "$DECUNE_FAKE_PLANNED_PORT" "$DECUNE_FAKE_PLANNED_PORT"
    exit 0
    ;;
esac
echo "unexpected fake docker command: $*" >&2
exit 64
