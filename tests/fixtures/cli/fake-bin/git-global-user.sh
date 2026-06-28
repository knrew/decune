#!/bin/sh
if [ "$1" = config ] && [ "$2" = --global ] && [ "$3" = --get ]; then
  case "$4" in
    user.name)
      printf 'Octo User\n'
      exit 0
      ;;
    user.email)
      printf 'octo@example.test\n'
      exit 0
      ;;
  esac
fi
exit 1
