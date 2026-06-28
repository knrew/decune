#!/bin/sh
out=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
if [ -z "$out" ] || [ -z "$url" ]; then
  echo "curl fake requires -o and url" >&2
  exit 64
fi
case "$url" in
  */SHA256SUMS)
    printf '%s\n' "0000000000000000000000000000000000000000000000000000000000000000  decune-v1.2.3-aarch64-apple-darwin.tar.gz" >"$out"
    ;;
  *)
    printf '%s\n' archive >"$out"
    ;;
esac
