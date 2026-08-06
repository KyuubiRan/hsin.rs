# Security model

- IPC endpoints are available only to the current operating-system user and validate the peer identity where supported.
- Provider secrets are encrypted with XChaCha20-Poly1305. SQLite stores ciphertext and key-check metadata; the operating-system keyring stores versioned master keys.
- A Linux system service has no session bus, so it takes its master key from a host-bound systemd credential instead of the Secret Service. That credential is read-only at runtime: writes fail loudly rather than landing somewhere the next start would not read back.
- Recovery keys are shown only on explicit export, accepted through hidden input or stdin, and never written to logs.
- Ordinary RPC responses expose masked previews only. The credential-helper RPC is the single narrow API that may return secret material.
- The proxy may bind to loopback, a specific interface, or a wildcard address. Every request validates a per-client capability token before replacing inbound authentication with the upstream credential; the fixed `HSIN_MANAGED_KEY` compatibility value is accepted only from loopback peers.
- Request bodies, authorization headers, provider secrets, recovery keys and raw IPC parameters are excluded from tracing.
- If the system keyring is unavailable, the daemon enters a locked state instead of falling back to plaintext storage.
