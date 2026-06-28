#!/bin/sh
archive=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -xzf)
      archive="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
if [ -z "$archive" ]; then
  echo "tar fake requires -xzf archive" >&2
  exit 64
fi
name="${archive##*/}"
root="${name%.tar.gz}"
mkdir -p "$root"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$root/decune"
chmod +x "$root/decune"
