#!/usr/bin/env bash

set -euo pipefail

has_shebang() {
  local file=$1
  local first_line

  IFS= read -r first_line < "$file" || return 1

  [[ $first_line == '#!'* ]]
}

is_sh_or_bash_shebang() {
  local file=$1
  local first_line

  IFS= read -r first_line < "$file" || return 1

  case "$first_line" in
    '#!/bin/sh' | '#!/bin/sh '* | '#!/usr/bin/sh' | '#!/usr/bin/sh '* | \
      '#!/usr/bin/env sh' | '#!/usr/bin/env sh '* | '#!/usr/bin/env -S sh' | '#!/usr/bin/env -S sh '* | \
      '#!/bin/bash' | '#!/bin/bash '* | '#!/usr/bin/bash' | '#!/usr/bin/bash '* | \
      '#!/usr/bin/env bash' | '#!/usr/bin/env bash '* | '#!/usr/bin/env -S bash' | '#!/usr/bin/env -S bash '*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_shell_extension() {
  local file=$1

  case "$file" in
    *.sh | *.bash)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_shell_dotfile() {
  local file=$1

  case "${file##*/}" in
    .bashrc | .bash_profile | .bash_login | .bash_logout | .bash_aliases | .profile)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

list_shell_files() {
  git ls-files -z | while IFS= read -r -d '' file; do
    if has_shebang "$file"; then
      if is_sh_or_bash_shebang "$file"; then
        printf '%s\0' "$file"
      fi
    elif is_shell_extension "$file" || is_shell_dotfile "$file"; then
      printf '%s\0' "$file"
    fi
  done
}

mapfile -d '' -t files < <(list_shell_files)

if [[ ${#files[@]} -eq 0 ]]; then
  printf "No sh/bash files found.\n"
  exit 0
fi

printf 'shellcheck:\n'
shellcheck "${files[@]}"

printf 'shfmt:\n'
shfmt -d "${files[@]}"
