#!/bin/sh
# Install the latest hsin release for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.sh | sh
#
# Environment:
#   HSIN_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   HSIN_VERSION      release tag to install, such as v0.2.0 (default: the latest)
set -eu

repo="KyuubiRan/hsin.rs"
destination="${HSIN_INSTALL_DIR:-${HOME}/.local/bin}"

case "$(uname -s)" in
  Darwin) system=apple-darwin ;;
  Linux) system=unknown-linux-gnu ;;
  *) echo "hsin has no build for $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) architecture=aarch64 ;;
  x86_64 | amd64) architecture=x86_64 ;;
  *) echo "hsin has no build for $(uname -m)" >&2; exit 1 ;;
esac

target="${architecture}-${system}"
if [ "$target" = "x86_64-apple-darwin" ]; then
  echo "Intel macOS has no published archive because GitHub retired its Intel" >&2
  echo "runners. Build it from a checkout with:" >&2
  echo "  ./scripts/build.sh release macos-x64" >&2
  exit 1
fi

archive="hsin-${target}.tar.gz"
if [ -n "${HSIN_VERSION:-}" ]; then
  base="https://github.com/${repo}/releases/download/${HSIN_VERSION}"
else
  # Asset names carry no version, so this URL keeps working across releases.
  base="https://github.com/${repo}/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
  # `--retry` alone ignores TLS handshake failures, which are exactly the
  # transient failures GitHub's asset host produces. `--retry-all-errors` covers
  # them but is curl 7.71+, so probe for it rather than hand an older curl an
  # option it will reject.
  if curl --help all 2>/dev/null | grep -q -- '--retry-all-errors'; then
    retry="--retry 3 --retry-all-errors"
  else
    retry="--retry 3"
  fi
  # shellcheck disable=SC2086 # $retry is a deliberate word-split option list.
  fetch() { curl -fsSL $retry "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q --tries=3 -O "$2" "$1"; }
else
  echo "need curl or wget" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo "need sha256sum or shasum to verify the download" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo "downloading ${archive}"
fetch "${base}/${archive}" "${work}/${archive}"
fetch "${base}/SHA256SUMS" "${work}/SHA256SUMS"

expected="$(grep " ${archive}\$" "${work}/SHA256SUMS" | cut -d' ' -f1)"
actual="$(digest "${work}/${archive}")"
if [ -z "$expected" ]; then
  echo "SHA256SUMS lists no digest for ${archive}" >&2
  exit 1
fi
if [ "$expected" != "$actual" ]; then
  echo "checksum mismatch for ${archive}" >&2
  echo "  expected ${expected}" >&2
  echo "  actual   ${actual}" >&2
  exit 1
fi

tar -xzf "${work}/${archive}" -C "$work"
mkdir -p "$destination"
# Replace by rename so a running daemon keeps its open image on Unix.
for binary in hsin hsind; do
  mv "${work}/hsin-${target}/${binary}" "${destination}/${binary}.new"
  chmod 755 "${destination}/${binary}.new"
  mv "${destination}/${binary}.new" "${destination}/${binary}"
done

echo "installed hsin and hsind into ${destination}"

case ":${PATH}:" in
  *":${destination}:"*) ;;
  *)
    echo
    echo "${destination} is not on your PATH. Add it, for example:"
    echo "  echo 'export PATH=\"${destination}:\$PATH\"' >> ~/.profile"
    ;;
esac

echo
echo "Run 'hsin' to start. It registers and starts the background daemon by"
echo "itself the first time it needs one."
