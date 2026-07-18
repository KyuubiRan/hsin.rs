# 心 / hsin

`hsin` is a daemon-first provider switcher for Codex and Claude Code. The daemon owns persistence, encrypted secrets, provider configuration, and the loopback proxy; the CLI and TUI communicate with it exclusively through local IPC.

The project is under active development. Version 0.1 targets macOS, Linux and Windows and intentionally excludes tray UI, Gemini, remote proxy binds and FoxSwitcher database migration.

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

Release archives are produced for:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

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
