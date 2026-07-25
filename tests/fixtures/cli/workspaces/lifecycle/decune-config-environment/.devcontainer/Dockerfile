FROM alpine:3.20
RUN printf '%s\n' \
  '#!/bin/sh' \
  'test "$PATH" = "/usr/bin:/bin:/extra" || exit 11' \
  'test "$DECUNE_DEFAULT_ENV" = "fallback" || exit 12' \
  'test "$DECUNE_REMOTE_ONLY" = "remote" || exit 13' \
  'printf "%s|%s|%s" "$PATH" "$DECUNE_DEFAULT_ENV" "$DECUNE_REMOTE_ONLY" >/tmp/decune-container-env-expansion' \
  >/usr/local/bin/decune-check-expanded-env \
  && chmod +x /usr/local/bin/decune-check-expanded-env
