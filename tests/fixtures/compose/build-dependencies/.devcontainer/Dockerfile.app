ARG BASE_IMAGE
FROM ${BASE_IMAGE}
RUN test -f /usr/local/share/decune/build-dependency-marker
