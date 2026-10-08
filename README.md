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

`hsin doctor` is read-only and never installs or restarts the daemon. On
Windows it can still report a missing release or installed `hsind.exe`, an
incomplete logon-task registration, and a possible security-software quarantine
when the daemon cannot be reached. It never restores quarantined files or adds
Windows Defender exclusions.

Debug builds use a `hsin-debug` data home, so a development daemon never shares
storage, keyring entries, the IPC endpoint, or the service identity with an
installed release build.

The client directories remain shared by default, so debug builds can exercise
the real Codex and Claude Code configuration. Only one hsin instance may manage
a client directory at a time. A conflicting write is rejected before changing
configuration or provider state. On entering the TUI, configuration owned by
another instance opens a takeover prompt with Cancel selected by default.
Cancelling keeps the interface available and does not prompt again on refresh.
Startup takeover transfers control without switching your selected provider;
conflicting actions later ask whether to take over and continue. Safe takeover
first restores the previous instance's preserved settings and authentication,
then transfers ownership. That instance will not reapply the relinquished client
on restart.
If the previous instance cannot safely restore its state, takeover is refused.
Configuration handoff negotiates its own stable release protocol, so compatible
instances can transfer ownership even when their release version codes differ.
This connection permits only `config.release`; ordinary CLI connections still
require an exact daemon version code and reinstall their own daemon on mismatch.
The v0.2.9 owner (version code 33) has a narrowly validated compatibility path.
An incompatible wire protocol or ownership format still requires an owner upgrade.

When upgrading an older instance, an existing hsin configuration may have no
ownership record yet. At startup, Codex configuration is migrated when its last
completed local write, provider, helper, current configuration and preserved
authentication can be verified together. Migration preserves the selected
provider and client files, including third-party API authentication; official
login is not required. An old local authentication backup alone is not proof
that it belongs to the current shared configuration. Unverified legacy state
still requires restoration by the previous managing instance or a fresh native
baseline before takeover. If an owner is offline, start it; if its key store is
locked, unlock it; if it lacks the handoff protocol, upgrade it before retrying.

## Usage

Run `hsin` with no arguments for the terminal UI. The mouse works alongside the
keyboard: click a client tab, a list row or a footer hint, click a selected row
again to activate it, and scroll with the wheel. Hold Shift while dragging to
select text in terminals that support it. Every action is also scriptable:

```bash
hsin status                                   # daemon, proxy and client state
hsin stats codex                              # last 30 days of Codex token usage
hsin stats codex --all                        # everything recorded, with plan quotas
hsin stats claude --from 2026-09-01 --to 2026-09-14
hsin stats codex --provider <id> --model gpt-5 --json
hsin pricing list                             # price rules used for cost estimates
hsin pricing set 'local-*' --input 2 --output 8 --currency CNY
hsin pricing refresh                          # fetch the public LiteLLM price list
hsin doctor                                   # configuration, security and service checks
hsin update                                   # update to the latest release

hsin provider import-current --client codex   # adopt what a client already uses
hsin provider add codex --name Example \
  --base-url https://api.example.com/v1 --secret-stdin
hsin provider list
hsin provider switch codex <provider-id>

hsin mode set codex proxy                     # direct or proxy
hsin config takeover codex                    # explicitly take over shared configuration
hsin config takeover codex claude             # take over both clients when necessary
hsin settings get
hsin security export-recovery-key             # keep this before you need it

hsin daemon status                            # also start, stop, restart, update
```

`--secret-stdin` reads the API key from standard input so it never appears in
process arguments or shell history. Add `--json` to any command for
machine-readable output, and `--language system|en-US|zh-CN` (or
`HSIN_LANGUAGE`) to override the interface language.

After a CLI takeover, rerun the original command. The CLI does not retain API
keys for retry or automatically take over configuration. `hsin status` reports
the configuration state and current managing instance for each client; another
instance's selected provider is not displayed as applied to the shared client.

Codex providers default their configuration name to `OpenAI`, enabling Codex's
remote-compaction path. The TUI switch can disable it by writing `hsin`, and the
same value can be set explicitly with `--config-name`. This changes only
`[model_providers.hsin].name`; the active selector and provider table key remain
`hsin`.

Settings -> Client configuration -> Codex configuration also provides an
opt-in **Preserve official login** switch. It restores and then leaves Codex's
native `auth.json` login untouched while third-party model requests obtain their
credential through `[model_providers.hsin.auth]`. Hsin never writes the real
provider key or an environment-variable value into Codex configuration. Enable
this only after signing in through the Official provider; enabling it also turns
Hsin Auth back on. Enabling **Disable custom Auth** later turns preservation off.

Codex and Claude Code support saved official accounts. On a client provider
list, press **Alt+A** to start OAuth using the installed official CLI. Codex uses
its app server; Claude uses `claude auth login --claudeai` and requires Claude
Code 2.1.126 or newer. Claude Pro, Max, Team, and Enterprise accounts are supported;
Console OAuth is excluded. Hsin shows the browser login progress and accepts a
Claude authorization code when needed. Cancel stops the isolated login and
removes its temporary credentials. Missing or incompatible CLIs produce an
upgrade hint; hsin does not install them.

Windows official-account operations require Codex 0.161 or newer so its native
encrypted secrets backend matches the account adapter. Native storage is based
on Codex 0.161 and Claude Code 2.1.293; real browser login and new-session account
switching still need platform acceptance before a release.

Login adds the account without changing your active provider. Close existing
client sessions, select a saved account, and press **Enter** to enable it for
new sessions. **e** edits its name; **d** removes an inactive saved account from
the local vault. Switch away before deleting the active account. Client settings
offer **Email** (default), **Name**, or **Name + email** display, with a fallback
when an account has no email or name.

Hsin automatically saves persistent native official logins in its encrypted
vault. API-only configurations do not create an official account. Signing in
again with the same identity and organization refreshes its credentials and
keeps your custom name. The fixed **Official** entry remains separate and
restores the native login present before account switching. Codex supports
file, keyring, and auto auth storage; Claude preserves unrelated credentials
and account configuration. External account changes produce a conflict so
hsin cannot silently overwrite the new login.

Each primary Codex Provider's add/edit form has **Context override**,
**Reasoning effort**, and **Plan mode reasoning effort** above
**Configure image generation**. The
context switch expands maximum context and auto-compaction threshold inline.
On either context field, Tab opens a sorted list of saved token presets and an
empty choice; values can also be entered directly. Settings -> Client
configuration -> Codex configuration manages separate lists for maximum context
(initially 272,000 and 1,000,000) and auto-compaction (initially 258,000 and
900,000). Press `a` to add or `e` to edit a token count in a dialog, or `d`
twice to delete one. Up/Down moves between form fields. When the
override is off, hsin leaves both keys untouched; when it is on, an empty field
removes its key. Both reasoning effort controls cycle independently through do
not modify, `low`, `medium`, `high`, `xhigh`, `ultra`, and `max`. Each Provider
saves its own choices and applies them on activation; do not modify leaves the
existing value alone.
New Codex Image provider selections prefer `gpt-image-2.5-sunburst` when the
provider lists it (then `gpt-image-2.5-flare`); the Claude Opus mapping suggests
`claude-opus-5-5` for a new, otherwise empty row.

Press `s` on a Codex or Claude Code TUI page to open token statistics. Hsin
combines exact usage observed by its local proxy with usage metadata from new
Codex and Claude Code session-log entries, so official-login and direct-mode
requests are included too. Collection begins after the upgraded daemon first
starts: old sessions are not backfilled. Request detail is kept for 90 days and
hourly totals indefinitely, so the overview opens on all time: an activity
heatmap of the last year (hover a day to preview it, click it for that day's
details), streaks, the favorite model, the peak hour, and an estimated cost per
currency. Press `d` to cycle all time, 7 and 30 days, or `t` for
other ranges. The model and daily charts split cache-hit input, other input and
output by shade, as stacked bars or, after `v`, as lines on a log scale; the
choice is saved. Provider attribution from local session logs is inferred from
Hsin's route history and is marked with `~`.

For a Codex or Claude subscription, the overview also estimates the plan's
allowance the way quota calculators do: Codex logs its quota meter with every
request and Claude Code caches its meter in `~/.claude.json`, so the
tokens spent while the meter moved, scaled to 100%, give the weekly allowance in
tokens and in API list-price dollars, with the range the whole-percent meter
leaves open, the remainder of the current window, and the equivalent per
calendar month. Each account, session provider, plan and window is estimated
separately, with its recent cycles listed, so switching between a relay and your
own plan, or between plans, never mixes their allowances. Plans active in the
last 30 days show by default; press `q` on the stats screen (or pass
`--all-plans`) to see older ones or pick one plan. It is an estimate;
OpenAI does not publish token limits.

Costs use built-in list prices, the public LiteLLM price list when you fetch it
(`u` on Settings → Model pricing, or `hsin pricing refresh`), and your own
rules, which always win and can be limited to one provider. Amounts are
estimates at today's prices and are never converted between currencies.

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
