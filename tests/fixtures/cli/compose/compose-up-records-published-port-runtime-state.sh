#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
project="decune-$DECUNE_FAKE_WORKSPACE_SLUG-$DECUNE_FAKE_WORKSPACE_ID"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","ports":[{"target":3000,"published":"%s","protocol":"tcp"}]}}}\n' "$DECUNE_FAKE_REQUESTED_PORT"
      exit 0
      ;;
    *" up -d "*)
      : > "$DECUNE_FAKE_UP_MARKER"
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"com.docker.compose.project=$project"*)
      if [ -f "$DECUNE_FAKE_UP_MARKER" ]; then
        printf '{"ID":"compose-app-id"}\n'
      fi
      exit 0
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Image":"sha256:alpine","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true},"NetworkSettings":{"Ports":{"3000/tcp":[{"HostIp":"0.0.0.0","HostPort":"%s"},{"HostIp":"::","HostPort":"%s"}]}}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project" "$DECUNE_FAKE_PLANNED_PORT" "$DECUNE_FAKE_PLANNED_PORT"
  exit 0
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
