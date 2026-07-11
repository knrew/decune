#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}},"networks":{"app":{"name":"fixed-network"}},"configs":{"app":{"name":"fixed-config"}},"secrets":{"app":{"name":"fixed-secret"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      echo "compose up should not run after clone isolation preflight failure" >&2
      exit 92
      ;;
  esac
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  printf 'fixed-network-id\n'
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = inspect ]; then
  printf '{"Name":"fixed-network","Driver":"bridge","Scope":"local","Labels":{"com.docker.compose.project":"other-project"},"IPAM":{"Driver":"default","Config":[]}}\n'
  exit 0
fi
if [ "${1:-}" = config ] && [ "${2:-}" = inspect ] && [ "${3:-}" = -- ] && [ "${4:-}" = fixed-config ]; then
  printf '[{"ID":"fixed-config-id","Spec":{"Name":"fixed-config","Labels":{"com.docker.compose.project":"other-project"}}}]\n'
  exit 0
fi
if [ "${1:-}" = secret ] && [ "${2:-}" = inspect ] && [ "${3:-}" = -- ] && [ "${4:-}" = fixed-secret ]; then
  printf '[{"ID":"fixed-secret-id","Spec":{"Name":"fixed-secret","Labels":{"com.docker.compose.project":"other-project"}}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
