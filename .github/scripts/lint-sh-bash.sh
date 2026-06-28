#!/usr/bin/env bash
set -euo pipefail

SHELLCHECK_VERSION="${SHELLCHECK_VERSION:-0.11.0}"
SHFMT_VERSION="${SHFMT_VERSION:-3.13.1}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

is_shell_file() {
  local path="$1"
  local first_line

  case "$path" in
    *.bash | *.sh)
      return 0
      ;;
  esac

  IFS= read -r first_line <"$path" || return 1
  [[ "$first_line" =~ ^#!.*(^|[/[:space:]])(bash|sh)([[:space:]]|$) ]]
}

require_command git
require_command shellcheck
require_command shfmt

detected_shellcheck_version="$(shellcheck --version | awk -F': ' '$1 == "version" { print $2 }')"
if [[ "$detected_shellcheck_version" != "$SHELLCHECK_VERSION" ]]; then
  fail "shellcheck ${SHELLCHECK_VERSION} is required; found ${detected_shellcheck_version:-unknown}"
fi

detected_shfmt_version="$(shfmt --version)"
detected_shfmt_version="${detected_shfmt_version#v}"
if [[ "$detected_shfmt_version" != "$SHFMT_VERSION" ]]; then
  fail "shfmt ${SHFMT_VERSION} is required; found ${detected_shfmt_version:-unknown}"
fi

files=()
while IFS= read -r -d '' path; do
  if [[ -f "$path" ]] && is_shell_file "$path"; then
    files+=("$path")
  fi
done < <(git ls-files -z)

if [[ "${#files[@]}" -eq 0 ]]; then
  printf 'No shell files found.\n'
  exit 0
fi

printf 'Checking shell files:\n'
printf '  %s\n' "${files[@]}"

shellcheck "${files[@]}"
shfmt -d "${files[@]}"
