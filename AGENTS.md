# Repository Guidelines

@/Users/kitsune/.codex/RTK.md

## Architecture invariants

- `hsind` is the only owner of SQLite, keyring access, client configuration writes and upstream provider secrets.
- `hsin` is an IPC client. Its only local side effect is daemon bootstrap/service lifecycle.
- Never modify client model, MCP, hooks, permissions, profiles, features or sandbox settings.
- The proxy is loopback-only in v1. Never log secrets, authorization headers, request bodies, recovery keys or databases.
- Prefix shell commands with `rtk`; use `apply_patch` for targeted file edits.

## Verification

Run formatting, Clippy and all workspace tests before committing. Configuration patch changes require preservation tests proving that non-owned fields remain byte-for-byte unchanged.

