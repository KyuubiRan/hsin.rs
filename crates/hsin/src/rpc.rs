use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hsin_core::{ClientKind, ConnectionMode, DaemonStatus, Provider, ProviderListParams};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::bootstrap;

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
        if let Ok(client) = Self::connect().await {
            return Ok(client);
        }

        bootstrap::install_and_start().await?;
        let mut last_error = None;
        for _ in 0..30 {
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(last_error.unwrap_or_else(|| anyhow!("daemon did not become ready")))
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

fn decode_status(value: &Value) -> Result<StatusSnapshot> {
    if let Ok(daemon) = serde_json::from_value::<DaemonStatus>(value.clone()) {
        let mut status = StatusSnapshot {
            security_locked: daemon.locked,
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
