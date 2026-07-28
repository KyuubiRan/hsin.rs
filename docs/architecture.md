# Architecture

```text
hsin CLI/TUI ── framed JSON-RPC over user-only IPC ── hsind
                                                       ├─ SQLite state
                                                       ├─ OS keyring master key
                                                       ├─ Codex/Claude patchers
                                                       └─ configurable HTTP proxy listener
```

`hsind` is the sole persistent state owner. The client never opens the database, reads the operating-system keyring, parses managed application configuration, or holds proxy routes. The daemon restores active routes before accepting control RPCs.

Each client has one active provider and one connection mode. A direct-mode switch uses a recoverable configuration saga. A proxy-mode switch commits state and swaps the in-memory route without touching external configuration. Requests retain the immutable provider snapshot captured when forwarding begins.

The proxy listening IP and port are daemon-owned settings. They can be changed while the listener is enabled; `hsind` rewrites active proxy-mode client endpoints and hot-restarts the listener. Wildcard listener addresses are converted to connectable loopback destinations in local client configuration (`0.0.0.0` becomes `127.0.0.1`, and `::` becomes `::1`). Daemon startup also reconciles proxy-mode client configuration so stale endpoints from an older binary are repaired automatically.

The only client-side bootstrap operation is launching `hsind service install --start` when the local IPC endpoint is absent. All provider, settings and security operations require a successful protocol handshake.

## Standalone (daemon-less) mode

For headless or single-binary deployments, `hsin` can embed the daemon core in-process instead of talking to `hsind`. Standalone mode activates when `--no-daemon` (or `HSIN_NO_DAEMON=true`) is set, or automatically when no `hsind` binary is deployed next to the CLI. The embedded core acquires the same exclusive instance lock as `hsind`, so state ownership stays unique: if a daemon is running, the IPC path is used; if the lock is held but IPC is unreachable, standalone mode refuses to start. Proxy mode requires the persistent daemon listener and is rejected in standalone mode with `proxy_requires_daemon`; direct mode, provider management, security, and credential-helper operations are fully supported.

## Key store backends

The master key lives in the operating-system keyring by default. Headless hosts without a keyring service can opt into a file-backed store (`HSIN_KEYSTORE=file`, or `hsind run --keystore file`) that keeps the master key in a user-only file under `<data home>/keys`; provider secrets remain encrypted in SQLite either way.

## Configuration ownership

- Codex: `model_provider` and the `model_providers.hsin` subtree.
- Claude Code: `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, and `apiKeyHelper`.

Everything else is outside hsin ownership. Patchers operate on source-preserving syntax trees and use compare-and-swap plus atomic replacement.
