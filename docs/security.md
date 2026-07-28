# Security model

- IPC endpoints are available only to the current operating-system user and validate the peer identity where supported.
- Provider secrets are encrypted with XChaCha20-Poly1305. SQLite stores ciphertext and key-check metadata; the operating-system keyring stores versioned master keys.
- Recovery keys are shown only on explicit export, accepted through hidden input or stdin, and never written to logs.
- Ordinary RPC responses expose masked previews only. The credential-helper RPC is the single narrow API that may return secret material.
- The proxy may bind to loopback, a specific interface, or a wildcard address. Every request validates a per-client capability token before replacing inbound authentication with the upstream credential; the fixed `HSIN_MANAGED_KEY` compatibility value is accepted only from loopback peers.
- Request bodies, authorization headers, provider secrets, recovery keys and raw IPC parameters are excluded from tracing.
- If the system keyring is unavailable, the daemon enters a locked state instead of falling back to plaintext storage.
- Headless hosts can explicitly opt into a file-backed master-key store with `HSIN_KEYSTORE=file` (or `hsind run --keystore file`). Secrets stay encrypted in SQLite; the master key is written to a user-only (0600) file inside the user-only data directory. This is never a silent fallback — without the opt-in, a missing keyring still locks the daemon.
- Standalone (`--no-daemon`) operation embeds the daemon core in the CLI process under the same exclusive instance lock, keeps the same encryption model, and rejects proxy mode because no persistent listener exists.
