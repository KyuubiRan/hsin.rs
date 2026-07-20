# AGENTS.md

@/Users/kitsune/.codex/RTK.md

## Project

`hsin` is a daemon-first provider switcher for Codex and Claude Code.

The Cargo workspace contains:

- `crates/hsin-core`: domain types, validation rules, and stable error codes.
- `crates/hsin-ipc`: versioned JSON-RPC types and local IPC transport.
- `crates/hsind`: the daemon and sole persistent-state owner.
- `crates/hsin`: the CLI and Ratatui TUI.

Version 1 supports direct and loopback-proxy modes on macOS, Linux, and Windows. Tray UI, Gemini, WebDAV, remote proxy binding, and FoxSwitcher migration are out of scope.

## Required Boundaries

- `hsind` exclusively owns SQLite, migrations, backups, keyring access, encrypted provider secrets, configuration writes, proxy routing, and daemon settings.
- `hsin` communicates with `hsind` through IPC. Its only non-IPC responsibility is daemon bootstrap and service lifecycle commands.
- Do not add SQLite, keyring, proxy-server, or client-config parsing dependencies to `hsin`.
- Keep public provider DTOs free of complete credentials. Only the internal `credential.resolve` flow may return credential material.
- Preserve stable IPC method names, wire enum values, protocol version checks, frame limits, request IDs, and hello negotiation.
- Whenever an updated `hsind` binary is shipped, increment the shared `hsin_core::VERSION_CODE` and ship the matching `hsin` binary with it. Daemon/CLI RPC fields, response shapes, capabilities, or required behavior changes always require this bump. A version-code mismatch must continue to trigger automatic daemon reinstallation; do not rely on the Cargo package version for development-build compatibility.
- `HSIN_HOME` instances must remain isolated across storage, IPC, keyring entries, installation markers, and service identities.

## Configuration Ownership

Configuration writes are strict allowlists:

- Codex `config.toml`: only top-level `model_provider` and the complete `[model_providers.hsin]` subtree.
- Codex `auth.json`: only top-level `auth_mode` and `OPENAI_API_KEY` while a custom Provider is active. Preserve and restore the prior values through encrypted daemon-owned backup state; never expose the backup through public DTOs or plaintext SQLite settings.
- Claude Code: only `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, and root `apiKeyHelper`.

Never modify model selection, `ANTHROPIC_MODEL`, MCP servers, hooks, permissions, profiles, features, approval policy, or sandbox settings.

Use source-preserving edits. Any config patch change must retain non-owned fields, comments, ordering, line endings, and Unicode byte-for-byte. Keep CAS checks, file locking, permission preservation, atomic replacement, and saga recovery intact.

While Hsin Auth remains enabled, custom Codex Providers use the command-backed credential helper for model requests and write only `HSIN_MANAGED_KEY` to `auth.json` to maintain Codex's API-key login state. When a client enables "Disable custom Auth", custom Providers use `requires_openai_auth = true`; direct mode writes the active Provider key to `auth.json`, while proxy mode still writes only `HSIN_MANAGED_KEY`. Switching to an Official Provider must restore the prior `auth_mode` and `OPENAI_API_KEY` values without changing tokens or other login fields.

## Security

- Never log or print provider secrets, recovery keys, authorization headers, request bodies, databases, or raw sensitive RPC parameters.
- Treat Codex `auth.json` and its encrypted backup as secret material. Never include either file's contents in operation JSON, logs, diagnostics, or error messages.
- Keep the proxy bound to loopback. Do not introduce `0.0.0.0` or remote listening in v1.
- Use `secrecy` and `zeroize` at sensitive in-memory boundaries where supported.
- Do not fall back to plaintext secret storage when the system keyring is unavailable. The daemon must remain recoverably locked.
- Keep credential helpers bound to provider identity and revision so stale client configurations cannot obtain a different provider's key.
- Proxy requests must capture one immutable provider and credential snapshot at request start.

## Rust Style

- Follow the existing Rust 2024 workspace patterns and shared workspace lints.
- `unsafe_code` is denied.
- Prefer existing domain DTOs and error codes over new ad hoc JSON shapes.
- Keep daemon mutations serialized and SQLite writes transactional.
- Avoid unrelated refactors, dependency churn, or generated metadata changes.
- Use `apply_patch` for targeted edits.

## Commands

Prefix shell commands with `rtk` as required by `/Users/kitsune/.codex/RTK.md`.

Required local gates:

```zsh
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test --workspace
```

Relevant cross-target checks:

```zsh
rtk cargo check --workspace --all-targets --target x86_64-apple-darwin
rtk cargo zigbuild --workspace --all-targets --target x86_64-unknown-linux-gnu
rtk cargo zigbuild --workspace --all-targets --target aarch64-unknown-linux-gnu
rtk cargo xwin check --workspace --all-targets --target x86_64-pc-windows-msvc
```

Configuration, IPC, crypto, proxy, service, or database changes require focused tests in addition to the workspace gates. Use temporary `HSIN_HOME`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR` values for integration tests, and remove test keyring entries afterward.

## Git

- Preserve unrelated user changes in a dirty worktree.
- Do not use destructive reset or checkout commands.
- Use English Conventional Commit messages, for example `feat: add tui settings menu` or `fix: preserve config comments`.
- Do not invent a license, repository URL, release tag, or public package metadata without explicit user approval.
