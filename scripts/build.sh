#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/build.sh [debug|release] [platform] [options]

Build hsin and hsind, then copy both binaries into one test directory.

Platforms:
  host            Current rustc host target (default)
  macos-arm64     aarch64-apple-darwin
  macos-x64       x86_64-apple-darwin
  linux-arm64     aarch64-unknown-linux-gnu
  linux-x64       x86_64-unknown-linux-gnu
  windows-x64     x86_64-pc-windows-msvc
  windows-arm64   aarch64-pc-windows-msvc
  <target-triple> Any installed Rust target triple

Options:
  --profile <name>   debug or release
  --platform <name>  Platform alias or target triple
  --target <triple>  Exact Rust target triple
  --output <path>    Destination directory
  --clean            Remove the destination directory before copying
  -h, --help         Show this help

Examples:
  scripts/build.sh
  scripts/build.sh release
  scripts/build.sh debug macos-x64
  scripts/build.sh release linux-x64
  scripts/build.sh --profile release --platform windows-x64
EOF
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

profile=debug
platform=host
target=
output=
clean=false
positional=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    debug|release)
      if [ "$positional" -eq 0 ]; then
        profile=$1
        positional=1
      else
        platform=$1
        positional=2
      fi
      shift
      ;;
    --profile)
      profile=${2:?--profile requires debug or release}
      shift 2
      ;;
    --platform)
      platform=${2:?--platform requires a value}
      shift 2
      ;;
    --target)
      target=${2:?--target requires a target triple}
      shift 2
      ;;
    --output)
      output=${2:?--output requires a path}
      shift 2
      ;;
    --clean)
      clean=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [ "$positional" -eq 0 ]; then
        platform=$1
        positional=2
      elif [ "$positional" -eq 1 ]; then
        platform=$1
        positional=2
      else
        echo "unexpected argument: $1" >&2
        usage >&2
        exit 2
      fi
      shift
      ;;
  esac
done

if [ "$profile" != debug ] && [ "$profile" != release ]; then
  echo "profile must be debug or release: $profile" >&2
  exit 2
fi

host=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$host" ]; then
  echo "could not determine the rustc host target" >&2
  exit 1
fi

if [ -z "$target" ]; then
  case "$platform" in
    host) target=$host ;;
    macos-arm64|mac-arm64|darwin-arm64) target=aarch64-apple-darwin ;;
    macos-x64|mac-x64|darwin-x64) target=x86_64-apple-darwin ;;
    linux-arm64) target=aarch64-unknown-linux-gnu ;;
    linux-x64) target=x86_64-unknown-linux-gnu ;;
    windows-x64|win-x64) target=x86_64-pc-windows-msvc ;;
    windows-arm64|win-arm64) target=aarch64-pc-windows-msvc ;;
    *) target=$platform ;;
  esac
fi

profile_flag=
if [ "$profile" = release ]; then
  profile_flag=--release
fi

build_tool=cargo
build_subcommand=build
if [ "$target" != "$host" ]; then
  case "$target" in
    *-unknown-linux-gnu)
      build_tool=cargo
      build_subcommand=zigbuild
      ;;
    *-pc-windows-msvc)
      build_tool=cargo
      build_subcommand=xwin
      if [ "$target" = aarch64-pc-windows-msvc ]; then
        # ring compiles its C sources with the GCC-style clang driver on Windows
        # AArch64, which rejects the /imsvc include flags that cargo-xwin's
        # default clang-cl backend emits.
        export XWIN_CROSS_COMPILER=clang
      fi
      ;;
  esac
fi

echo "host:    $host"
echo "target:  $target"
echo "profile: $profile"
echo "builder: $build_tool $build_subcommand"

if [ "$target" = "$host" ]; then
  if [ -n "$profile_flag" ]; then
    "$build_tool" "$build_subcommand" --workspace "$profile_flag"
  else
    "$build_tool" "$build_subcommand" --workspace
  fi
  source_dir="$repo_root/target/$profile"
else
  if [ "$build_subcommand" = xwin ]; then
    if [ -n "$profile_flag" ]; then
      "$build_tool" xwin build --workspace --target "$target" "$profile_flag"
    else
      "$build_tool" xwin build --workspace --target "$target"
    fi
  elif [ -n "$profile_flag" ]; then
    "$build_tool" "$build_subcommand" --workspace --target "$target" "$profile_flag"
  else
    "$build_tool" "$build_subcommand" --workspace --target "$target"
  fi
  source_dir="$repo_root/target/$target/$profile"
fi

if [ -z "$output" ]; then
  output="$repo_root/artifacts/$target/$profile"
elif [ "${output#/}" = "$output" ]; then
  output="$repo_root/$output"
fi

if [ "$clean" = true ]; then
  rm -rf -- "$output"
fi
mkdir -p -- "$output"

suffix=
case "$target" in
  *-windows-*) suffix=.exe ;;
esac

for binary in hsin hsind; do
  source="$source_dir/$binary$suffix"
  if [ ! -f "$source" ]; then
    echo "missing build output: $source" >&2
    exit 1
  fi
  cp -f -- "$source" "$output/$binary$suffix"
done

cp -f -- "$repo_root/README.md" "$output/README.md"
cp -f -- "$repo_root/LICENSE" "$output/LICENSE"

echo
echo "build outputs:"
ls -lh "$output/hsin$suffix" "$output/hsind$suffix"
echo "directory: $output"
