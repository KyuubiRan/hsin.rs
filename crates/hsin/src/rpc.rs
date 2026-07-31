use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hsin_core::{
    ClientKind, ConnectionMode, DaemonStatus, ErrorCode, Provider, ProviderListParams,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::bootstrap;

const DAEMON_READY_RETRIES: usize = 300;
const DAEMON_READY_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusSnapshot {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub codex_active_provider: Option<String>,
    #[serde(default)]
    pub claude_active_provider: Option<String>,
    #[serde(default = "direct")]
    pub codex_mode: ConnectionMode,
    #[serde(default = "direct")]
    pub claude_mode: ConnectionMode,
    #[serde(default)]
    pub proxy_enabled: bool,
    #[serde(default)]
    pub security_locked: bool,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            codex_active_provider: None,
            claude_active_provider: None,
            codex_mode: ConnectionMode::Direct,
            claude_mode: ConnectionMode::Direct,
            proxy_enabled: false,
            security_locked: false,
        }
    }
}

const fn direct() -> ConnectionMode {
    ConnectionMode::Direct
}

pub struct DaemonClient {
    inner: tokio::sync::Mutex<hsin_ipc::IpcClient>,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self> {
        let mut inner = hsin_ipc::IpcClient::connect_default()
            .await
            .context("connect to hsind")?;
        inner
            .hello(&hsin_ipc::HelloParams::new(
                "hsin",
                env!("CARGO_PKG_VERSION"),
            ))
            .await
            .context("negotiate hsind protocol")?;
        Ok(Self {
            inner: tokio::sync::Mutex::new(inner),
        })
    }

    pub async fn connect_or_bootstrap() -> Result<Self> {
        let initial_error = match Self::connect().await {
            Ok(client) => return Ok(client),
            Err(error) => error,
        };

        if requires_reinstall(&initial_error) {
            bootstrap::install_and_start().await?;
            return Self::wait_until_ready(Some(initial_error)).await;
        }

        if bootstrap::service_status().await.unwrap_or(false) {
            return Self::wait_for_running_daemon(initial_error).await;
        }

        bootstrap::install_and_start().await?;
        Self::wait_until_ready(Some(initial_error)).await
    }

    async fn wait_for_running_daemon(mut last_error: anyhow::Error) -> Result<Self> {
        for _ in 0..DAEMON_READY_RETRIES {
            tokio::time::sleep(DAEMON_READY_RETRY_DELAY).await;
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(error) if requires_reinstall(&error) => {
                    bootstrap::install_and_start().await?;
                    return Self::wait_until_ready(Some(error)).await;
                }
                Err(error) => last_error = error,
            }
        }
        Err(unreachable_daemon(last_error.context(
            "hsind service is running but IPC did not become ready",
        )))
    }

    async fn wait_until_ready(mut last_error: Option<anyhow::Error>) -> Result<Self> {
        for _ in 0..DAEMON_READY_RETRIES {
            tokio::time::sleep(DAEMON_READY_RETRY_DELAY).await;
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(error) => last_error = Some(error),
            }
        }
        Err(unreachable_daemon(
            last_error.unwrap_or_else(|| anyhow!("daemon did not become ready")),
        ))
    }

    pub async fn call<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.inner
            .lock()
            .await
            .call(method, params)
            .await
            .with_context(|| format!("RPC {method} failed"))
    }

    pub async fn provider_list(&self, client: Option<ClientKind>) -> Result<Vec<Provider>> {
        let value: Value = self
            .call("provider.list", &ProviderListParams { client })
            .await?;
        if value.is_array() {
            return serde_json::from_value(value).context("decode provider list");
        }
        serde_json::from_value(
            value
                .get("providers")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
        .context("decode provider list")
    }

    pub async fn status(&self) -> Result<StatusSnapshot> {
        let value: Value = self.call("status", &json!({})).await?;
        decode_status(&value)
    }
}

/// A missing socket only says the daemon is not listening. Point at the daemon
/// log so a crash-looping service reports its own failure instead of a bare
/// "No such file or directory".
fn unreachable_daemon(error: anyhow::Error) -> anyhow::Error {
    let log = hsin_ipc::data_home().join("logs").join("hsind.stderr.log");
    error.context(format!(
        "hsind is not reachable and may be failing to start; see {}",
        log.display()
    ))
}

fn requires_reinstall(error: &anyhow::Error) -> bool {
    error.chain().any(
        |source| match source.downcast_ref::<hsin_ipc::TransportError>() {
            Some(
                hsin_ipc::TransportError::ProtocolMismatch { .. }
                | hsin_ipc::TransportError::VersionCodeMismatch { .. },
            ) => true,
            Some(hsin_ipc::TransportError::Rpc(rpc)) => rpc
                .data
                .as_ref()
                .is_some_and(|application| application.code == ErrorCode::ProtocolMismatch),
            _ => false,
        },
    )
}

fn decode_status(value: &Value) -> Result<StatusSnapshot> {
    if let Ok(daemon) = serde_json::from_value::<DaemonStatus>(value.clone()) {
        let mut status = StatusSnapshot {
            security_locked: daemon.locked,
            proxy_enabled: daemon.proxy_enabled,
            ..StatusSnapshot::default()
        };
        for client in daemon.clients {
            if client.client == ClientKind::Codex {
                status.codex_active_provider = client.active_provider_id;
                status.codex_mode = client.mode;
            } else {
                status.claude_active_provider = client.active_provider_id;
                status.claude_mode = client.mode;
            }
        }
        return Ok(status);
    }
    if let Ok(status) = serde_json::from_value::<StatusSnapshot>(value.clone()) {
        return Ok(status);
    }

    // Keep the TUI compatible with a daemon that returns client state as a map.
    let mut status = StatusSnapshot::default();
    if let Some(clients) = value.get("clients") {
        decode_client_state(clients.get("codex"), &mut status, true)?;
        decode_client_state(clients.get("claude"), &mut status, false)?;
    }
    status.security_locked = value
        .pointer("/security/locked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    status.proxy_enabled = value
        .get("proxy_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(status)
}

fn decode_client_state(
    value: Option<&Value>,
    status: &mut StatusSnapshot,
    codex: bool,
) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let active = value
        .get("active_provider_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mode = value
        .get("mode")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("decode connection mode")?
        .unwrap_or(ConnectionMode::Direct);
    if codex {
        status.codex_active_provider = active;
        status.codex_mode = mode;
    } else {
        status.claude_active_provider = active;
        status.claude_mode = mode;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_compatibility_failures_require_daemon_reinstallation() {
        let mismatch = anyhow::Error::new(hsin_ipc::TransportError::VersionCodeMismatch {
            expected: hsin_ipc::VERSION_CODE,
            actual: hsin_ipc::VERSION_CODE.saturating_sub(1),
        })
        .context("negotiate hsind protocol");
        assert!(requires_reinstall(&mismatch));

        let rejected_by_old_daemon = anyhow::Error::new(hsin_ipc::TransportError::Rpc(
            hsin_ipc::RpcError::application(hsin_core::AppError::new(ErrorCode::ProtocolMismatch)),
        ))
        .context("negotiate hsind protocol");
        assert!(requires_reinstall(&rejected_by_old_daemon));

        let unavailable = anyhow::Error::new(hsin_ipc::TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "socket is not ready",
        )))
        .context("connect to hsind");
        assert!(!requires_reinstall(&unavailable));
    }
}
