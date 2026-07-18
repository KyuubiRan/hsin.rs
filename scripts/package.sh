#!/bin/sh
set -eu

target="${1:?usage: scripts/package.sh <target>}"
metadata="$(cargo metadata --no-deps --format-version 1)"
version="$(printf '%s\n' "$metadata" | sed -n 's/.*\"name\":\"hsin\",\"version\":\"\([^\"]*\)\".*/\1/p' | head -n 1)"
if [ -z "$version" ]; then
  echo "could not determine the hsin package version" >&2
  exit 1
fi
root="dist/hsin-${version}-${target}"

rm -rf "$root"
mkdir -p "$root"
cp "target/${target}/release/hsin" "$root/hsin"
cp "target/${target}/release/hsind" "$root/hsind"
cp README.md "$root/README.md"

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
