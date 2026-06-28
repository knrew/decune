#!/bin/sh
case " $* " in
  *" version --short "*)
    printf '2.40.0\n'
    exit 0
    ;;
  *" version "*)
    printf 'Docker Compose version v2.40.0\n'
    exit 0
    ;;
  *" config --help "*)
    printf '%s\n' 'Usage: docker compose config [OPTIONS]' '      --format string'
    exit 0
    ;;
  *" ps --help "*)
    printf '%s\n' 'Usage: docker compose ps [OPTIONS]' '      --format string'
    exit 0
    ;;
  *" build --help "*)
    printf '%s\n' 'Usage: docker compose build [OPTIONS]' '      --with-dependencies'
    exit 0
    ;;
  *" pull --help "*)
    printf '%s\n' 'Usage: docker compose pull [OPTIONS]' '      --policy string' '      --ignore-buildable' '      --include-deps'
    exit 0
    ;;
  *" up --help "*)
    printf '%s\n' 'Usage: docker compose up [OPTIONS]' '      --force-recreate' '      --remove-orphans'
    exit 0
    ;;
esac
