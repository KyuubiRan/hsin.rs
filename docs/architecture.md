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

Ordinary CLI hello checks the exact daemon version code. Owner-to-owner configuration handoff instead requests `config_handoff.v1` on a connection restricted to `config.release`, then verifies the wire protocol, release capability and recorded owner identity. Known v0.2.9/code-33 owners may retry hello with their legacy code; other old versions cannot use that fallback. Ownership format, generation, CAS and restoration checks remain in the release/claim operations.

## Configuration ownership

- Codex: `model_provider`, the `model_providers.hsin` subtree, and the four optional top-level tuning keys `model_context_window`, `model_auto_compact_token_limit`, `model_reasoning_effort`, and `plan_mode_reasoning_effort`. Each primary Codex Provider persists its own optional Context override and two reasoning efforts. Activating it writes the context keys only when the override is enabled (empty removes its key); disabled means neither key is modified. Selected reasoning efforts write their keys independently; each default “do not modify” leaves its key untouched. With official-auth preservation disabled, hsin also owns only the top-level `auth_mode` and `OPENAI_API_KEY` fields in `auth.json`; preservation restores those fields once and then leaves the file untouched.
- Claude Code: `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_API_KEY`, `env.ANTHROPIC_AUTH_TOKEN`, and `apiKeyHelper`.
- Claude Code model mapping (opt-in per provider): `env.ANTHROPIC_DEFAULT_FABLE_MODEL`, `env.ANTHROPIC_DEFAULT_OPUS_MODEL`, `env.ANTHROPIC_DEFAULT_SONNET_MODEL`, and `env.ANTHROPIC_DEFAULT_HAIKU_MODEL`. The 1M-context option is written as a `[1m]` suffix on the model ID. Whatever the user had in these keys before hsin first wrote them is snapshotted and restored for any tier that is not mapped, so disabling the mapping is non-destructive. `ANTHROPIC_MODEL` is never touched.

Everything else is outside hsin ownership. Patchers operate on source-preserving syntax trees and use compare-and-swap plus atomic replacement.

## Saved official accounts

`official_accounts` binds a Provider to a stable native identity and organization, a public email/name summary, and an independent credential revision. OAuth snapshots are encrypted in `protected_values`; they are never API-key credentials or public Provider fields. The fixed Official provider remains a separate native-login recovery entry. Existing persistent native logins are captured automatically; API-only and ephemeral authentication produce no saved account.

The `official_accounts.v1` IPC capability adds `official_account.login.start`, `.status`, `.submit`, `.cancel`, and `official_account.rename`. The daemon launches an installed official CLI in a private temporary home and owns login cancellation, timeout, orphan cleanup, credential capture, and vault writes. The TUI opens the browser and renders progress. Successful login saves or refreshes the account while retaining the active Provider. `settings.update` accepts a per-client `official_account_display` update; switching and deletion use the existing Provider methods.

Official-account writes extend the native-auth allowlist to Codex's `auth_mode`, `OPENAI_API_KEY`, `tokens`, and `last_refresh`, or Claude's `claudeAiOauth` credential subtree and `oauthAccount` metadata subtree. Codex file/keyring/auto and Windows encrypted secrets retain unrelated records. Claude respects native directory locks and the Keychain namespace derived from the explicit configuration directory. Third-party configuration writes retain their existing allowlists.

Switch journals reference encrypted immutable before/after snapshots. Ownership fingerprints use stable identity and organization so token refreshes remain valid; each write also checks current credentials. Independent credential revisions prevent old refresh results from overwriting a later login. Cooperative handoff restores the latest native credentials and reserves their identity until the receiving instance claims them. Account selection applies to newly started client sessions.

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

Every insert marks the local days it touches in `usage_rollup_dirty`, in the
same transaction. Before a sync finishes or a query runs, those days are rebuilt
into `usage_hourly` (per day, hour, Provider revision, and model). Rollups
outlive the raw events, so reports read only from them: an all-time query
(`from` = 0) starts at the first recorded day, and a report also carries a
371-day activity calendar, streaks, and the peak hour.

Codex `token_count` records also carry the plan's quota windows (`used_percent`,
window length, reset time, plan type). Each is stored in `usage_quota_readings`
with the tokens of the same record, for 35 days. Readings naming the same reset
time form one cycle; concurrent sessions interleave slightly stale percentages,
so a cycle's movement is the rise of its running peak over its first reading,
and only usage up to the last rise counts. A window's allowance is
Σ tokens × 100 / Σ movement over the recent cycles, and each cycle adds one
point of uncertainty to the reported range. Readings also record the session's
provider (`session_meta.model_provider`) and a hash of its account, and each
account, provider, plan and window is estimated on its own: a user can move
between plans and accounts, and relay sessions carry no quota readings at all.
Windows missing from the newest readings are reported as no longer in use.

Claude Code caches its plan usage (weekly window, reset time, and Claude
Code's share of it) in its global config; its five-hour window resets too often
to estimate from and is not read. Every
refresh is a reading, credited with the official-login Claude requests since
the previous reading of the same window; relay requests are left out, and the
weekly movement is scaled by Claude Code's share because chat and other apps
draw on the same allowance. Quota readings are kept 90 days; the overview shows
plans active in the last 30 days unless another plan filter is chosen. Session cursors keep the Codex parser state (current
model and cumulative counters) so a sync that resumes mid-file still knows the
model; a one-time background pass relabels events an earlier version filed
under `unknown`.

Costs are estimates from `model_prices` plus a built-in table: a
Provider-scoped rule beats a general one, a user rule beats a fetched one which
beats a built-in one, and an exact model beats the longest matching prefix.
Model names are compared lowercase without a `vendor/` prefix, a `[1m]` suffix,
or a release date. Each currency is summed on its own; tokens no rule matches
are reported as unpriced.
