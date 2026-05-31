#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${DECUNE_CONTAINER_TOOLS_OUT:-"$root/target/container-tools"}
RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols"
export RUSTFLAGS

if [ "$#" -eq 0 ]; then
    set -- linux-amd64 linux-arm64
fi

rm -rf "$out"
mkdir -p "$out"
: >"$out/SHA256SUMS"
manifest="$out/manifest.json"

printf '{\n  "version": 1,\n  "protocolVersion": 1,\n  "tools": [\n' >"$manifest"
first=1

for platform in "$@"; do
    case "$platform" in
        linux-amd64) target=x86_64-unknown-linux-musl ;;
        linux-arm64) target=aarch64-unknown-linux-musl ;;
        *)
            echo "Unsupported container tool platform: $platform" >&2
            exit 1
            ;;
    esac

    case "$target" in
        aarch64-unknown-linux-musl)
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-rust-lld} \
                cargo build --release --target "$target" -p decune-container-tools --bins
            ;;
        *)
            cargo build --release --target "$target" -p decune-container-tools --bins
            ;;
    esac
    mkdir -p "$out/$platform"

    for tool in git-credential-decune decune-forward-agent; do
        source="$root/target/$target/release/$tool"
        dest="$out/$platform/$tool"
        cp "$source" "$dest"
        chmod 0755 "$dest"
        sha=$(sha256sum "$dest" | awk '{print $1}')
        printf '%s  %s/%s\n' "$sha" "$platform" "$tool" >>"$out/SHA256SUMS"
        if [ "$first" -eq 1 ]; then
            first=0
        else
            printf ',\n' >>"$manifest"
        fi
        printf '    {"name":"%s","platform":"%s","path":"%s/%s","sha256":"%s"}' \
            "$tool" "$platform" "$platform" "$tool" "$sha" >>"$manifest"
    done
done

printf '\n  ]\n}\n' >>"$manifest"
