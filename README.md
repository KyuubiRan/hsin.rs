# 心 / hsin

`hsin` is a daemon-first provider switcher for Codex and Claude Code. The daemon owns persistence, encrypted secrets, provider configuration, and the loopback proxy; the CLI and TUI communicate with it exclusively through local IPC.

The project is under active development.

## Workspace

- `hsin-core`: domain types and stable errors
- `hsin-ipc`: versioned local RPC protocol and transport
- `hsind`: daemon, storage, secrets, configuration and proxy
- `hsin`: CLI and terminal UI

