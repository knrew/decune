FROM alpine:3.20
ENV DECUNE_FROM_IMAGE=from-image
RUN printf '%s\n' \
  '#!/bin/sh' \
  'test "$PATH" = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/attach-extra" || exit 11' \
  'test "$DECUNE_IMAGE_ENV" = "from-image" || exit 12' \
  'printf "%s|%s" "$PATH" "$DECUNE_IMAGE_ENV" >/tmp/decune-attached-container-env-expansion' \
  'exit 0' \
  >/usr/local/bin/decune-check-attached-env \
  && chmod +x /usr/local/bin/decune-check-attached-env
