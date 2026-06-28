#!/bin/sh
set -eu
if [ "${1:-}" = compose ] && [ -n "${DECUNE_FAKE_COMPOSE_CAPABILITIES:-}" ]; then
  # shellcheck disable=SC1090
  . "$DECUNE_FAKE_COMPOSE_CAPABILITIES"
fi
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"example/app:dev"},"sidecar":{"image":"example/sidecar:dev"}}}\n'
      exit 0
      ;;
    *" build "*)
      case "$*" in
        *"--with-dependencies --no-cache --pull"*) exit 0 ;;
      esac
      echo "compose build did not receive --with-dependencies, --no-cache, and --pull: $*" >&2
      exit 42
      ;;
    *" pull "*)
      exit 0
      ;;
    *" up -d "*)
      previous=
      generated_override=
      for argument in "$@"; do
        if [ "$previous" = "-f" ]; then
          case "$argument" in
            *compose.override.yaml) generated_override=$argument ;;
          esac
        fi
        previous=$argument
      done
      test -n "$generated_override"
      cat "$generated_override" > "$DECUNE_FAKE_OVERRIDE_LOG"
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = build ]; then
  case "$*" in
    *"--tag decune/"*"--pull"*)
      echo "Generated Feature build must not receive --pull: $*" >&2
      exit 43
      ;;
    *"--tag decune/"*"--no-cache"*) cat >/dev/null; exit 0 ;;
  esac
  echo "Feature build did not receive --no-cache: $*" >&2
  exit 43
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:test","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = pull ]; then
  printf '{"status":"pulled"}\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = pull ]; then
  printf '{"status":"pulled"}\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
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
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
