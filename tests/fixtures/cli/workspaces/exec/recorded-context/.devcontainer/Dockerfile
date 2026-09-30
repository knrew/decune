FROM alpine:3.20
COPY recorded-login-shell.sh /usr/local/bin/decune-recorded-login-shell
RUN chmod +x /usr/local/bin/decune-recorded-login-shell \
    && adduser -D -s /usr/local/bin/decune-recorded-login-shell decune
