#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = build ]; then
  case "$*" in
    *"-base --file"*)
      case "$*" in
        *"--no-cache"*"--pull"*)
          cat >/dev/null
          exit 0
          ;;
      esac
      echo "User Dockerfile build did not receive --no-cache and --pull: $*" >&2
      exit 42
      ;;
    *)
      case "$*" in
        *"--pull"*)
          echo "Generated Docker build must not receive --pull: $*" >&2
          exit 43
          ;;
        *"--no-cache"*)
          cat >/dev/null
          exit 0
          ;;
      esac
      echo "Generated Docker build did not receive --no-cache: $*" >&2
      exit 44
      ;;
  esac
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:test","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'fake-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"fake-container-id","Name":"/decune-fake","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"fake-container-id","Name":"/decune-fake","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
