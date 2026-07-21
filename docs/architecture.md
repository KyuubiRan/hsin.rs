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

## Configuration ownership

- Codex: `model_provider` and the `model_providers.hsin` subtree.
- Claude Code: `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, and `apiKeyHelper`.

Everything else is outside hsin ownership. Patchers operate on source-preserving syntax trees and use compare-and-swap plus atomic replacement.
