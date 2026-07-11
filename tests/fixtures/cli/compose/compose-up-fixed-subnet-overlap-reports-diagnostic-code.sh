#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","networks":["grpc"]}},"networks":{"grpc":{"ipam":{"config":[{"subnet":"172.28.0.0/16","gateway":"172.28.0.1"}]}}}}\n'
      exit 0
      ;;
    *" up -d "*)
      echo "compose up should not run after clone isolation preflight failure" >&2
      exit 92
      ;;
  esac
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  printf 'other-network-id\n'
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = inspect ]; then
  printf '{"Name":"other_grpc","Driver":"bridge","Scope":"local","Labels":{"com.docker.compose.project":"other-project"},"IPAM":{"Driver":"default","Config":[{"Subnet":"172.28.10.0/24","Gateway":"172.28.10.1"}]}}\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
