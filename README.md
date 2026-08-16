# 心 / hsin

WIP

## Workspace

- `hsin-core`: domain types and stable errors
- `hsin-ipc`: versioned local RPC protocol and transport
- `hsind`: daemon, storage, secrets, configuration and proxy
- `hsin`: CLI and terminal UI

## Install

### macOS

```sh
brew install KyuubiRan/tap/hsin
```

Upgrades then follow `brew upgrade` along with everything else you have
installed. The script below works on macOS too if you would rather not use
Homebrew.

### Linux, and macOS without Homebrew

```sh
curl -fsSL https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.sh | sh
```

Installs into `~/.local/bin`, and tells you if that is not on your `PATH`. Set
`HSIN_INSTALL_DIR` to install elsewhere, or `HSIN_VERSION` to pin a tag such as
`v0.2.0`. The archive is checked against the release's `SHA256SUMS` before
anything is installed.

### Windows

```powershell
irm https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.ps1 | iex
```

Installs into `%LOCALAPPDATA%\Programs\hsin`, adds it to your user `PATH`, and
makes `hsin` available in the current PowerShell session immediately. It takes
the same `HSIN_INSTALL_DIR` and `HSIN_VERSION` overrides and performs the same
checksum check. Nothing here needs an elevated prompt.

### Update

```bash
hsin update
```

The command checks GitHub's latest release before downloading anything. On
macOS, when the running `hsin` binary belongs to the installed Homebrew formula,
it updates through `brew upgrade`; merely having Homebrew installed does not
change a script or manual installation. Linux, Windows, and non-Homebrew macOS
installations use the same checksum-verified scripts shown above. An existing
background service is updated immediately with the new daemon binary.

### Manual download

Both scripts do nothing you cannot do by hand: pick the archive for your
platform from [Releases](https://github.com/KyuubiRan/hsin.rs/releases), verify
it against `SHA256SUMS`, and put `hsin` and `hsind` on your `PATH`.

| Platform | Archive |
| --- | --- |
| macOS, Apple Silicon | `hsin-aarch64-apple-darwin.tar.gz` |
| Linux, x86-64 | `hsin-x86_64-unknown-linux-gnu.tar.gz` |
| Linux, ARM64 | `hsin-aarch64-unknown-linux-gnu.tar.gz` |
| Windows, x86-64 | `hsin-x86_64-pc-windows-msvc.zip` |
| Windows, ARM64 | `hsin-aarch64-pc-windows-msvc.zip` |

Asset names carry no version — the tag does — so a download URL keeps working
across releases. Each archive holds a single folder containing both binaries.

Intel macOS has no published archive because GitHub retired its Intel macOS
runners; build it locally with `./scripts/build.sh release macos-x64`.

### Register the background service

```bash
hsin daemon install --start
```

This is optional: any `hsin` command bootstraps the daemon when the local IPC
endpoint is absent, so running `hsin` on its own is enough. Installing
explicitly is what registers the definition that starts the daemon at login.

Either way, `install` copies both binaries into the data home and registers the
service from there, so the copy under `<data home>/bin` is the one that runs.

| | Data home | Service definition |
| --- | --- | --- |
| macOS | `~/Library/Application Support/hsin` | launchd agent `~/Library/LaunchAgents/dev.hsin.hsind.<scope>.plist` |
| Linux | `$XDG_DATA_HOME/hsin`, else `~/.local/share/hsin` | systemd **user** unit `~/.config/systemd/user/hsind-<scope>.service` |
| Windows | `%LOCALAPPDATA%\hsin` | Task Scheduler logon task `dev.hsin.hsind.<scope>` |

None of the three needs administrator or root rights. Two platform notes:

- **Linux** user units need a session bus and an unlocked keyring, and stop at
  logout unless you run `loginctl enable-linger "$USER"`. Headless servers
  should use the system service in
  [docs/linux-system-service.md](docs/linux-system-service.md) instead, which is
  the one case that does need root.
- **Windows** registers a logon-triggered task scoped to the installing account.
  The daemon is the task's own process, so Task Scheduler starts and stops it
  directly.

Debug builds use a `hsin-debug` data home, so a development daemon never shares
storage, keyring entries, the IPC endpoint, or the service identity with an
installed release build.

## Usage

Run `hsin` with no arguments for the terminal UI. Every action is also
scriptable:

```bash
hsin status                                   # daemon, proxy and client state
hsin doctor                                   # configuration, security and service checks
hsin update                                   # update to the latest release

hsin provider import-current --client codex   # adopt what a client already uses
hsin provider add codex --name Example \
  --base-url https://api.example.com/v1 --secret-stdin
hsin provider list
hsin provider switch codex <provider-id>

hsin mode set codex proxy                     # direct or proxy
hsin settings get
hsin security export-recovery-key             # keep this before you need it

hsin daemon status                            # also start, stop, restart, update
```

`--secret-stdin` reads the API key from standard input so it never appears in
process arguments or shell history. Add `--json` to any command for
machine-readable output, and `--language system|en-US|zh-CN` (or
`HSIN_LANGUAGE`) to override the interface language.

Codex providers default their configuration name to `OpenAI`, enabling Codex's
remote-compaction path. The TUI switch can disable it by writing `hsin`, and the
same value can be set explicitly with `--config-name`. This changes only
`[model_providers.hsin].name`; the active selector and provider table key remain
`hsin`.

Set `HSIN_HOME` to run isolated instances; each one keeps its own storage, IPC
endpoint, keyring entries and service identity. `CODEX_HOME` and
`CLAUDE_CONFIG_DIR` redirect the managed client configuration in the same way.

### Uninstall

```bash
hsin daemon uninstall            # remove the service, keep providers and keys
hsin daemon uninstall --purge    # also remove the data home and keyring entries
```

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

Release archives are produced on native GitHub Actions runners for the
platforms listed under [Install](#release-archive-all-supported-platforms).
`scripts/build.sh` additionally builds `x86_64-apple-darwin` locally.

Install the standard libraries and local cross-build helpers with:

```zsh
rustup component add rustfmt clippy
rustup target add \
  aarch64-apple-darwin x86_64-apple-darwin \
  aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu \
  x86_64-pc-windows-msvc aarch64-pc-windows-msvc
brew install zig
cargo install cargo-zigbuild cargo-xwin
```

`rustup target add` installs Rust's target standard library, not a linker. Use
plain Cargo for macOS, `cargo zigbuild` for Linux, and `cargo xwin` for Windows
MSVC cross-builds.

Cross-building `aarch64-pc-windows-msvc` additionally needs
`XWIN_CROSS_COMPILER=clang`. `ring` forces the GNU-driver `clang` for its
Windows AArch64 C sources, so cargo-xwin has to emit `-imsvc` include flags
instead of clang-cl's `/imsvc`. The build scripts set it for you; a bare `cargo
xwin` invocation does not.

See [docs/architecture.md](docs/architecture.md) and [docs/security.md](docs/security.md) for the process and trust boundaries.
