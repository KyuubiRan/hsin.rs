#!/bin/sh
set -eu

target="${1:?usage: scripts/package.sh <target>}"
# The tag carries the version. Keeping it out of the archive name gives every
# release the same asset names, so a download URL never needs rewriting.
root="dist/hsin-${target}"

rm -rf "$root"
mkdir -p "$root"
cp "target/${target}/release/hsin" "$root/hsin"
cp "target/${target}/release/hsind" "$root/hsind"
cp README.md "$root/README.md"
cp LICENSE "$root/LICENSE"

binary_bytes="$(wc -c < "$root/hsin")"
daemon_bytes="$(wc -c < "$root/hsind")"
combined_bytes="$((binary_bytes + daemon_bytes))"
if [ "$combined_bytes" -gt 26214400 ]; then
  echo "stripped binaries exceed 25 MiB combined: ${combined_bytes} bytes" >&2
  exit 1
fi

tar -C dist -czf "${root}.tar.gz" "$(basename "$root")"

size_kib="$(du -k "${root}.tar.gz" | awk '{print $1}')"
if [ "$size_kib" -gt 15360 ]; then
  echo "release archive exceeds 15 MiB: ${size_kib} KiB" >&2
  exit 1
fi

archive="$(basename "${root}.tar.gz")"
if command -v sha256sum >/dev/null 2>&1; then
  (cd dist && sha256sum "$archive" >"${archive}.sha256")
else
  (cd dist && shasum -a 256 "$archive" >"${archive}.sha256")
fi
