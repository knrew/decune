#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: install.sh --version <version> [--dir <install-dir>]

Installs decune from GitHub Releases.

Options:
  --version <version>  Release version, with or without a leading v.
  --dir <install-dir>  Install directory. Defaults to DECUNE_INSTALL_DIR or /usr/local/bin.
  -h, --help           Show this help.
EOF
}

version=""
install_dir="${DECUNE_INSTALL_DIR:-/usr/local/bin}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      if [ "$#" -lt 2 ]; then
        echo "error: --version requires a value" >&2
        exit 2
      fi
      version="$2"
      shift 2
      ;;
    --dir)
      if [ "$#" -lt 2 ]; then
        echo "error: --dir requires a value" >&2
        exit 2
      fi
      install_dir="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$version" ]; then
  echo "error: --version is required" >&2
  usage >&2
  exit 2
fi

case "$version" in
  v*) tag="$version"; version="${version#v}" ;;
  *) tag="v$version" ;;
esac

case "$(uname -s)" in
  Linux) os_target="unknown-linux-musl" ;;
  Darwin) os_target="apple-darwin" ;;
  *)
    echo "error: unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch_target="x86_64" ;;
  arm64 | aarch64) arch_target="aarch64" ;;
  *)
    echo "error: unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

target="${arch_target}-${os_target}"
archive="decune-v${version}-${target}.tar.gz"
base_url="https://github.com/knrew/decune/releases/download/${tag}"
tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/decune.XXXXXXXXXX")"

cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT INT TERM

if command -v curl >/dev/null 2>&1; then
  curl -fsSL -o "$tmpdir/$archive" "$base_url/$archive"
  curl -fsSL -o "$tmpdir/SHA256SUMS" "$base_url/SHA256SUMS"
elif command -v wget >/dev/null 2>&1; then
  wget -q -O "$tmpdir/$archive" "$base_url/$archive"
  wget -q -O "$tmpdir/SHA256SUMS" "$base_url/SHA256SUMS"
else
  echo "error: curl or wget is required" >&2
  exit 1
fi

(
  cd "$tmpdir"
  if ! grep "  $archive$" SHA256SUMS > SHA256SUMS.selected; then
    echo "error: checksum entry not found for $archive" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c SHA256SUMS.selected
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c SHA256SUMS.selected
  else
    echo "error: sha256sum or shasum is required" >&2
    exit 1
  fi
  tar -xzf "$archive"
)

if [ ! -d "$install_dir" ]; then
  echo "error: install directory does not exist: $install_dir" >&2
  exit 1
fi

if [ ! -w "$install_dir" ]; then
  echo "error: install directory is not writable: $install_dir" >&2
  echo "Run with a writable --dir value or rerun this script with appropriate privileges." >&2
  exit 1
fi

install -m 0755 "$tmpdir/decune-v${version}-${target}/decune" "$install_dir/decune"
echo "Installed decune $version to $install_dir/decune"
