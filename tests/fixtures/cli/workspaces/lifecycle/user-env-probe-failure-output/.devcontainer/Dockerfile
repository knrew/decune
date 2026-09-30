FROM alpine:3.20
COPY probe-failing-login-shell.sh /usr/local/bin/decune-probe-failing-login-shell
RUN chmod +x /usr/local/bin/decune-probe-failing-login-shell \
    && adduser -D -s /usr/local/bin/decune-probe-failing-login-shell decune \
    && printf '#!/bin/sh\nexit 0\n' >/usr/local/bin/decune-attached-shell \
    && chmod +x /usr/local/bin/decune-attached-shell
