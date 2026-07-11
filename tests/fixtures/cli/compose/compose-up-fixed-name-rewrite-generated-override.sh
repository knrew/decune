#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      if [ -n "${DECUNE_FAKE_SUBNET_RELOCATION:-}" ]; then
        printf '{"services":{"app":{"image":"alpine:3.20","networks":{"grpc":null}}},"networks":{"grpc":{"ipam":{"config":[{"subnet":"10.99.0.0/25","gateway":"10.99.0.1"}]}}}}\n'
      else
        printf '{"services":{"app":{"image":"alpine:3.20","container_name":"fixed-app","networks":{"default":null},"volumes":["cache:/cache"]}},"volumes":{"cache":{"name":"fixed-cache"}}}\n'
      fi
      exit 0
      ;;
    *" up -d "*)
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  if [ -n "${DECUNE_FAKE_OCCUPIED_SUBNET:-}" ]; then
    printf 'occupied-network-id\n'
  fi
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = inspect ]; then
  printf '{"Name":"occupied_grpc","Driver":"bridge","Scope":"local","Labels":{"com.docker.compose.project":"other-project","com.docker.compose.network":"grpc"},"Containers":{},"IPAM":{"Driver":"default","Config":[{"Subnet":"%s"}]}}\n' "$DECUNE_FAKE_OCCUPIED_SUBNET"
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ] && [ "${3:-}" = -- ]; then
  if [ "${4:-}" != "$DECUNE_FAKE_EXPECTED_CONTAINER_NAME" ]; then
    echo "unexpected container name probe: ${4:-}" >&2
    exit 93
  fi
  echo "Error response from daemon: No such container: ${4:-}" >&2
  exit 1
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ] && [ "${3:-}" = -- ]; then
  if [ "${4:-}" != "$DECUNE_FAKE_EXPECTED_VOLUME_NAME" ]; then
    echo "unexpected volume name probe: ${4:-}" >&2
    exit 94
  fi
  echo "Error response from daemon: No such volume: ${4:-}" >&2
  exit 1
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
if [ "${1:-}" = inspect ] || { [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; }; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
