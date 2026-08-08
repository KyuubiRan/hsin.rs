# AGENTS.md

## Project

`hsin` is a daemon-first provider switcher for Codex and Claude Code. It supports direct and local-proxy connection modes on macOS, Linux, and Windows.

The Cargo workspace contains:

- `crates/hsin-core`: domain types, validation, and stable error codes.
- `crates/hsin-ipc`: versioned JSON-RPC types and local IPC transport.
- `crates/hsind`: the daemon and sole persistent-state owner.
- `crates/hsin`: the CLI and Ratatui TUI.

## Ownership Boundaries

- `hsind` exclusively owns SQLite, migrations, backups, keyring access, encrypted provider secrets, configuration writes, proxy routing, and daemon settings.
- `hsin` communicates with `hsind` through IPC. Its only non-IPC responsibility is daemon bootstrap and service lifecycle commands.
- Do not add SQLite, keyring, proxy-server, or client-config parsing dependencies to `hsin`.
- Keep public provider DTOs free of complete credentials. Only `credential.resolve` may return credential material.
- Preserve IPC method names, wire enum values, protocol checks, frame limits, request IDs, and hello negotiation.
- Daemon/CLI RPC contract changes require incrementing `hsin_core::VERSION_CODE` and shipping matching `hsind` and `hsin` binaries. Version-code mismatches must continue to reinstall the daemon automatically.
- `HSIN_HOME` instances must remain isolated across storage, IPC, keyring entries, installation markers, and service identities.

## Configuration And Authentication

Configuration writes are strict allowlists:

- Codex `config.toml`: top-level `model_provider` and the complete `[model_providers.hsin]` subtree only.
- Codex `auth.json`: top-level `auth_mode` and `OPENAI_API_KEY` only while a custom provider is active.
- Claude Code: `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, root `apiKeyHelper`, and — only when the active provider enables model mapping — `env.ANTHROPIC_MODEL` plus, for each of the `FABLE`, `OPUS`, `SONNET`, and `HAIKU` tiers, `env.ANTHROPIC_DEFAULT_<TIER>_MODEL`, `env.ANTHROPIC_DEFAULT_<TIER>_MODEL_NAME`, and `env.ANTHROPIC_DEFAULT_<TIER>_MODEL_DESCRIPTION` (thirteen keys, enumerated by `CLAUDE_MODEL_ENV_KEYS`).

Never modify MCP servers, hooks, permissions, profiles, features, approval policy, or sandbox settings. Claude Code model selection is owned only through the thirteen keys above; `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION*`, `ANTHROPIC_DEFAULT_*_MODEL_SUPPORTED_CAPABILITIES`, and the root `model` key stay untouched. `ANTHROPIC_MODEL` is owned because Claude Code resolves the startup model as `--model` > `ANTHROPIC_MODEL` > the selection persisted in `settings.json`: without it a stale persisted first-party model ID outranks the tier mapping and reaches a provider that never heard of it. The user's own values for every owned key are snapshotted before hsin first writes them and restored whenever hsin has no value of its own; a snapshot taken before a key was owned records which keys it covers, so keys added later are captured rather than lost.

- Preserve non-owned fields, comments, ordering, line endings, and Unicode byte-for-byte.
- Retain CAS checks, file locking, permission preservation, atomic replacement, and operation recovery.
- Managed Codex Auth uses the daemon-backed credential helper and writes only `HSIN_MANAGED_KEY` to `auth.json`.
- With custom Auth disabled, direct mode writes the active provider key while proxy mode still writes only `HSIN_MANAGED_KEY`.
- Switching to an official provider restores the prior Codex `auth_mode` and `OPENAI_API_KEY` without changing unrelated login fields.
- Importing an official Codex provider restores any daemon-owned auth backup before synchronization and preserves the native official `config.toml` representation.
- Treat Claude Code as official OAuth only when the base URL is official and its API key, auth token, and `apiKeyHelper` are absent or empty. Never execute a detected `apiKeyHelper` during import.

## Security

- Never log or print provider secrets, recovery keys, authorization headers, request bodies, databases, or raw sensitive RPC parameters.
- Treat Codex `auth.json` and its encrypted backup as secret material.
- Non-loopback proxy requests require the random client capability; never accept the fixed `HSIN_MANAGED_KEY` from a non-loopback peer.
- Do not fall back to plaintext secret storage when the system keyring is unavailable. The daemon must remain recoverably locked.
- A Linux system-scope service reads its master key from a systemd credential. That store is read-only at runtime; never make a write to it appear to succeed.
- Bind credential helpers to provider identity and revision.
- Capture one immutable provider and credential snapshot at proxy request start.
- Use `secrecy` and `zeroize` at sensitive in-memory boundaries where supported.

## Engineering

- Follow the Rust 2024 workspace patterns and shared workspace lints.
- `unsafe_code` is denied.
- Prefer existing domain DTOs and error codes over ad hoc JSON shapes.
- Serialize daemon mutations and keep SQLite writes transactional.
- Keep changes scoped; avoid unrelated refactors, dependency churn, and generated metadata changes.
- Use `apply_patch` for targeted edits.

## Validation

Run these local gates for implementation changes:

```zsh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run relevant cross-target checks when the change affects portability or packaging:

```zsh
cargo check --workspace --all-targets --target x86_64-apple-darwin
cargo zigbuild --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo zigbuild --workspace --all-targets --target aarch64-unknown-linux-gnu
cargo xwin check --workspace --all-targets --target x86_64-pc-windows-msvc
```

Configuration, IPC, crypto, proxy, service, or database changes require focused tests in addition to the workspace gates. Use temporary `HSIN_HOME`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR` values for integration tests, then remove test keyring entries.

Once the gates pass, produce the binaries with the build script rather than a bare `cargo build`:

```zsh
scripts/build.sh
```

It builds the workspace and copies `hsin` and `hsind` into `artifacts/<target>/<profile>/`, so the change can be exercised as a real binary. Pass `release` for an optimized build and a platform alias (`macos-arm64`, `macos-x64`, `linux-x64`, `windows-x64`, …) to cross-build; `scripts/build.ps1` is the Windows equivalent.

## Git

- Preserve unrelated user changes in a dirty worktree.
- Do not use destructive reset or checkout commands.
- Use English Conventional Commit messages.
- Append `[skip ci]` to a commit that changes nothing CI can verify, such as a version bump whose code already passed on the preceding commit. GitHub skips `push` and `pull_request` workflows for it; `workflow_dispatch` releases still run.
- Do not invent a license, repository URL, release tag, or public package metadata without explicit approval.

## Releasing

- Bump `version` in the workspace `Cargo.toml`, refresh `Cargo.lock`, and commit it alone as `chore: release X.Y.Z [skip ci]`.
- Wait for CI to pass on the commit *before* the version bump. The bump itself carries no code, so it is not covered by CI.
- Run the `Release` workflow with `tag=vX.Y.Z`. It refuses to publish when the tag does not match the workspace version, creates the tag on the dispatched commit when it does not exist yet, and reuses the tag when re-running after a partial failure.
- Do not create the tag by hand. A tag pushed from a workstation runs CI a second time on the same commit and can point at the wrong one.
- Releases are pre-releases by default; clear the `prerelease` input deliberately.
