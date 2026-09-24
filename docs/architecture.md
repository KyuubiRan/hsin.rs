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

- Codex: `model_provider`, the `model_providers.hsin` subtree, and the four optional top-level tuning keys `model_context_window`, `model_auto_compact_token_limit`, `model_reasoning_effort`, and `plan_mode_reasoning_effort`. Each primary Codex Provider persists its own optional Context override and two reasoning efforts. Activating it writes the context keys only when the override is enabled (empty removes its key); disabled means neither key is modified. Selected reasoning efforts write their keys independently; each default “do not modify” leaves its key untouched. With official-auth preservation disabled, hsin also owns only the top-level `auth_mode` and `OPENAI_API_KEY` fields in `auth.json`; preservation restores those fields once and then leaves the file untouched.
- Claude Code: `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, and `apiKeyHelper`.
- Claude Code model mapping (opt-in per provider): `env.ANTHROPIC_DEFAULT_FABLE_MODEL`, `env.ANTHROPIC_DEFAULT_OPUS_MODEL`, `env.ANTHROPIC_DEFAULT_SONNET_MODEL`, and `env.ANTHROPIC_DEFAULT_HAIKU_MODEL`. The 1M-context option is written as a `[1m]` suffix on the model ID. Whatever the user had in these keys before hsin first wrote them is snapshotted and restored for any tier that is not mapped, so disabling the mapping is non-destructive. `ANTHROPIC_MODEL` is never touched.

Everything else is outside hsin ownership. Patchers operate on source-preserving syntax trees and use compare-and-swap plus atomic replacement.

## Usage statistics

`hsind` normalizes Token usage from two sources. Proxy responses supply exact
Provider snapshots without delaying streaming data; bounded observers recognize
Anthropic Messages events, OpenAI Responses completion events, compatible
stream-final usage, and bounded non-streaming JSON. Incremental readers scan
only usage metadata from new Codex and Claude Code JSONL records, allowing
official-login and direct-mode requests to be counted without reading or
storing prompts and responses.

The daemon records successful Provider/mode transitions in `usage_routes`.
Session events are matched against the route active at their timestamp and are
marked `inferred`; events before a known route remain `unattributed`. Proxy
events are `exact` and take precedence during cross-source deduplication.
Collection begins when a schema-9 daemon first starts, with existing complete
lines marked as read. File cursors contain only a normalized-path hash, byte
offset, modification metadata, and a tail fingerprint. Events older than the
local-calendar 90-day boundary are removed.
