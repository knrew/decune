#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = network ] || [ "${1:-}" = volume ] || [ "${1:-}" = secret ] || [ "${1:-}" = config ]; then
  echo "docker resource inspection should not include unselected Compose resources" >&2
  exit 92
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ] && [ "${4:-}" = fixed-unused ]; then
  echo "docker container inspection should not include an unselected Compose service" >&2
  exit 92
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json app "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"},"unused":{"image":"alpine:3.20","container_name":"fixed-unused","networks":["unused-network"],"volumes":["unused-volume:/data"]}},"networks":{"unused-network":{"ipam":{"config":[{"subnet":"172.28.0.0/16"}]}}},"volumes":{"unused-volume":{"name":"fixed-unused-volume"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      echo "selected Compose up reached after isolation preflight" >&2
      exit 73
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ] || [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
