# 心 / hsin

WIP

## Workspace

- `hsin-core`: domain types and stable errors
- `hsin-ipc`: versioned local RPC protocol and transport
- `hsind`: daemon, storage, secrets, configuration and proxy
- `hsin`: CLI and terminal UI

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run `hsind run` during development, then open the TUI with `hsin` or use the scriptable commands exposed by `hsin --help`.

For frequent local builds, use the host-aware helper. It defaults to a native
debug build and copies `hsin` and `hsind` into one directory under `artifacts/`:

```zsh
./scripts/build.sh
./scripts/build.sh release
./scripts/build.sh debug macos-x64
./scripts/build.sh release linux-x64
./scripts/build.sh --profile release --platform windows-x64
```

On Windows PowerShell, use the equivalent script:

```powershell
./scripts/build.ps1
./scripts/build.ps1 release
./scripts/build.ps1 release windows-x64
./scripts/build.ps1 -Profile release -Platform linux-x64
```

Release archives are produced for:

- `aarch64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Intel macOS has no published archive because GitHub retired its Intel macOS
runners. `scripts/build.sh` still builds `x86_64-apple-darwin` locally.

Install the standard libraries and local cross-build helpers with:

```zsh
rustup component add rustfmt clippy
rustup target add \
  aarch64-apple-darwin x86_64-apple-darwin \
  aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu \
  x86_64-pc-windows-msvc
brew install zig
cargo install cargo-zigbuild cargo-xwin
```

`rustup target add` installs Rust's target standard library, not a linker. Use
plain Cargo for macOS, `cargo zigbuild` for Linux, and `cargo xwin` for Windows
MSVC cross-builds. Tagged releases are still built and smoke-tested on native
GitHub Actions runners.

See [docs/architecture.md](docs/architecture.md) and [docs/security.md](docs/security.md) for the process and trust boundaries.
