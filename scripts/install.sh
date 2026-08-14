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
  Darwin)
    system=apple-darwin
    service_home="${HSIN_HOME:-${HOME}/Library/Application Support/hsin}"
    ;;
  Linux)
    system=unknown-linux-gnu
    service_home="${HSIN_HOME:-${XDG_DATA_HOME:-${HOME}/.local/share}/hsin}"
    ;;
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
  # shellcheck disable=SC2086 # $retry is a deliberate word-split option list.
  latest_release_url() { curl -fsSL $retry -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q --tries=3 -O "$2" "$1"; }
  latest_release_url() {
    wget --spider --server-response --tries=3 "$1" 2>&1 |
      awk 'tolower($1) == "location:" && $2 ~ /\/releases\/tag\// { gsub("\r", "", $2); print $2; exit }'
  }
else
  echo "need curl or wget" >&2
  exit 1
fi

if [ -n "${HSIN_VERSION:-}" ]; then
  release_tag="$HSIN_VERSION"
else
  release_url="$(latest_release_url "https://github.com/${repo}/releases/latest")"
  case "$release_url" in
    */releases/tag/*) release_tag="${release_url##*/}" ;;
    *) echo "could not determine the latest hsin release" >&2; exit 1 ;;
  esac
fi

normalize_version() {
  case "$1" in
    v*) printf '%s\n' "${1#v}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

current_version="${HSIN_CURRENT_VERSION:-}"
if [ -n "$current_version" ] &&
  [ "$(normalize_version "$current_version")" = "$(normalize_version "$release_tag")" ]; then
  echo "hsin $(normalize_version "$current_version") is already the latest release"
  exit 0
fi

service_installed=false
if [ -f "${service_home}/.hsin-home" ] || [ -f "${service_home}/bin/hsind" ]; then
  service_installed=true
fi

update_daemon_if_installed() {
  if [ "$service_installed" = true ]; then
    echo "updating the existing background daemon"
    "$1" daemon update
  fi
}

# A Homebrew installation must stay owned by Homebrew. Merely having brew on
# the machine is not enough: the running hsin must resolve inside the installed
# formula prefix, otherwise this is a script/manual installation.
if [ "$system" = apple-darwin ] && [ -z "${HSIN_VERSION:-}" ] &&
  command -v brew >/dev/null 2>&1; then
  executable="${HSIN_EXECUTABLE:-}"
  if [ -z "$executable" ]; then
    executable="$(command -v hsin 2>/dev/null || true)"
  fi
  brew_prefix="$(brew --prefix hsin 2>/dev/null || true)"
  if [ -n "$executable" ] && [ -d "$brew_prefix" ]; then
    executable_directory="$(cd -P "$(dirname "$executable")" && pwd)"
    brew_directory="$(cd -P "$brew_prefix" && pwd)"
    case "${executable_directory}/" in
      "${brew_directory}/"*)
        echo "updating Homebrew hsin to ${release_tag}"
        brew update
        brew upgrade KyuubiRan/tap/hsin
        updated_cli="$(brew --prefix hsin)/bin/hsin"
        updated_version="$("$updated_cli" --version)"
        updated_version="${updated_version#hsin }"
        if [ "$(normalize_version "$updated_version")" != "$(normalize_version "$release_tag")" ]; then
          echo "Homebrew installed hsin ${updated_version}, expected ${release_tag}" >&2
          exit 1
        fi
        update_daemon_if_installed "$updated_cli"
        echo "updated hsin to ${updated_version} with Homebrew"
        exit 0
        ;;
    esac
  fi
fi

archive="hsin-${target}.tar.gz"
base="https://github.com/${repo}/releases/download/${release_tag}"

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

echo "downloading ${archive} from ${release_tag}"
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

echo "installed hsin and hsind ${release_tag} into ${destination}"
update_daemon_if_installed "${destination}/hsin"

case ":${PATH}:" in
  *":${destination}:"*) ;;
  *)
    echo
    echo "${destination} is not on your PATH. Add it, for example:"
    echo "  echo 'export PATH=\"${destination}:\$PATH\"' >> ~/.profile"
    ;;
esac

if [ "$service_installed" = false ]; then
  echo
  echo "Run 'hsin' to start. It registers and starts the background daemon by"
  echo "itself the first time it needs one."
fi
