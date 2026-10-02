use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use hsin_core::{
    ClientKind, ConfigOwnershipStatus, ConfigStatus, ConfigTakeoverParams, ConfigTakeoverTarget,
    ConnectionMode, DaemonStatus, ErrorCode, Provider, ProviderListParams, SecurityStatus,
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
    #[serde(default)]
    pub codex_image_active_provider: Option<String>,
    #[serde(default = "direct")]
    pub codex_mode: ConnectionMode,
    #[serde(default = "direct")]
    pub claude_mode: ConnectionMode,
    #[serde(default)]
    pub proxy_enabled: bool,
    #[serde(default)]
    pub security_locked: bool,
    /// False until the operator has exported (or re-imported) a recovery key.
    /// Defaults to held so a daemon that cannot report it raises no false alarm.
    #[serde(default = "assume_held")]
    pub recovery_key_exported: bool,
    #[serde(default)]
    pub codex_config_status: Option<ConfigStatus>,
    #[serde(default)]
    pub claude_config_status: Option<ConfigStatus>,
    #[serde(default)]
    pub codex_config_ownership: Option<ConfigOwnershipStatus>,
    #[serde(default)]
    pub claude_config_ownership: Option<ConfigOwnershipStatus>,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            codex_active_provider: None,
            claude_active_provider: None,
            codex_image_active_provider: None,
            codex_mode: ConnectionMode::Direct,
            claude_mode: ConnectionMode::Direct,
            proxy_enabled: false,
            security_locked: false,
            recovery_key_exported: true,
            codex_config_status: None,
            claude_config_status: None,
            codex_config_ownership: None,
            claude_config_ownership: None,
        }
    }
}

impl StatusSnapshot {
    pub fn config_status(&self, client: ClientKind) -> Option<ConfigStatus> {
        if self
            .config_ownership(client)
            .is_some_and(|ownership| ownership.owner.is_some() && !ownership.owner_is_self)
        {
            return Some(ConfigStatus::Conflict);
        }
        match client {
            ClientKind::Codex => self.codex_config_status,
            ClientKind::Claude => self.claude_config_status,
        }
    }

    pub fn config_ownership(&self, client: ClientKind) -> Option<&ConfigOwnershipStatus> {
        match client {
            ClientKind::Codex => self.codex_config_ownership.as_ref(),
            ClientKind::Claude => self.claude_config_ownership.as_ref(),
        }
    }

    pub fn configuration_applied(&self, client: ClientKind) -> bool {
        self.config_status(client)
            .is_none_or(|status| status == ConfigStatus::Synchronized)
            && self
                .config_ownership(client)
                .is_none_or(|ownership| ownership.owner.is_none() || ownership.owner_is_self)
    }
}

pub fn config_takeover_params(targets: &[ConfigOwnershipStatus]) -> ConfigTakeoverParams {
    static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    ConfigTakeoverParams {
        request_id: format!(
            "{}-{time}-{}",
            std::process::id(),
            NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
        ),
        targets: targets
            .iter()
            .map(|ownership| ConfigTakeoverTarget {
                client: ownership.client,
                target_id: ownership.target_id.clone(),
                expected_owner_id: ownership
                    .owner
                    .as_ref()
                    .map(|owner| owner.instance_id.clone()),
                expected_generation: ownership.generation,
            })
            .collect(),
    }
}

const fn direct() -> ConnectionMode {
    ConnectionMode::Direct
}

const fn assume_held() -> bool {
    true
}

pub struct DaemonClient {
    inner: tokio::sync::Mutex<hsin_ipc::IpcClient>,
    daemon_version: String,
    required_capabilities_supported: bool,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self> {
        let mut inner = hsin_ipc::IpcClient::connect_default()
            .await
            .context("connect to hsind")?;
        let hello = inner
            .hello(&hsin_ipc::HelloParams::new(
                "hsin",
                env!("CARGO_PKG_VERSION"),
            ))
            .await
            .context("negotiate hsind protocol")?;
        Ok(Self {
            inner: tokio::sync::Mutex::new(inner),
            daemon_version: hello.daemon_version,
            required_capabilities_supported: has_required_capabilities(&hello.capabilities),
        })
    }

    pub async fn connect_or_bootstrap() -> Result<Self> {
        let initial_error = match Self::connect().await {
            Ok(client) => return client.reinstall_if_stale().await,
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

    /// A daemon that still speaks the same protocol answers the handshake, so an
    /// upgrade that changed no RPC field leaves the previously installed daemon
    /// serving every command, and its service definition starts that same old
    /// binary again at the next logon. Version codes only move when the wire
    /// contract does, so the build identity is what has to be compared: without
    /// this, a release that fixes only daemon behaviour never reaches anyone who
    /// already had hsin installed.
    async fn reinstall_if_stale(self) -> Result<Self> {
        if !daemon_needs_reinstall(&self.daemon_version, self.required_capabilities_supported) {
            return Ok(self);
        }
        // Reinstalling stops the daemon, so let go of the connection first.
        drop(self);
        bootstrap::install_and_start().await?;
        let client = Self::wait_until_ready(None).await?;
        anyhow::ensure!(
            client.required_capabilities_supported,
            "installed hsind does not support required configuration capabilities"
        );
        Ok(client)
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

    pub async fn security_status(&self) -> Result<SecurityStatus> {
        self.call("security.status", &json!({})).await
    }
}

fn daemon_needs_reinstall(version: &str, required_capabilities_supported: bool) -> bool {
    version != env!("CARGO_PKG_VERSION") || !required_capabilities_supported
}

fn has_required_capabilities(capabilities: &[String]) -> bool {
    [
        hsin_ipc::capability::CONTEXT_PRESETS,
        hsin_ipc::capability::PLAN_MODE_REASONING,
        hsin_ipc::capability::CONFIG_OWNERSHIP,
    ]
    .iter()
    .all(|required| capabilities.iter().any(|capability| capability == required))
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
            codex_image_active_provider: daemon.codex_image_active_provider_id,
            ..StatusSnapshot::default()
        };
        for client in daemon.clients {
            if client.client == ClientKind::Codex {
                status.codex_active_provider = client.active_provider_id;
                status.codex_mode = client.mode;
                status.codex_config_status = Some(client.config_status);
                status.codex_config_ownership = client.config_ownership;
            } else {
                status.claude_active_provider = client.active_provider_id;
                status.claude_mode = client.mode;
                status.claude_config_status = Some(client.config_status);
                status.claude_config_ownership = client.config_ownership;
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
    status.codex_image_active_provider = value
        .get("codex_image_active_provider_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
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
    let config_status = value
        .get("config_status")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("decode configuration state")?;
    let ownership = value
        .get("config_ownership")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("decode configuration ownership")?;
    if codex {
        status.codex_active_provider = active;
        status.codex_mode = mode;
        status.codex_config_status = config_status;
        status.codex_config_ownership = ownership;
    } else {
        status.claude_active_provider = active;
        status.claude_mode = mode;
        status.claude_config_status = config_status;
        status.claude_config_ownership = ownership;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_retains_configuration_conflict_and_binds_takeover_to_the_snapshot() {
        let status = decode_status(&json!({
            "version": env!("CARGO_PKG_VERSION"),
            "locked": false,
            "proxy_listening": false,
            "proxy_address": "127.0.0.1:9999",
            "clients": [{
                "client": "codex",
                "active_provider_id": "local-provider",
                "mode": "direct",
                "config_status": "conflict",
                "config_ownership": {
                    "client": "codex",
                    "target_id": "canonical-target",
                    "config_path": "/tmp/codex/config.toml",
                    "generation": 42,
                    "owner": {
                        "instance_id": "other-instance",
                        "instance_home": "/tmp/hsin-other",
                        "instance_label": "Release",
                        "daemon_version": env!("CARGO_PKG_VERSION")
                    },
                    "owner_is_self": false,
                    "takeover_available": true
                }
            }]
        }))
        .expect("decode daemon status");
        assert_eq!(status.codex_config_status, Some(ConfigStatus::Conflict));
        assert!(!status.configuration_applied(ClientKind::Codex));
        assert_eq!(
            status.codex_active_provider.as_deref(),
            Some("local-provider")
        );
        let request = config_takeover_params(&[status.codex_config_ownership.expect("owner")]);
        assert_eq!(
            request.targets[0].expected_owner_id.as_deref(),
            Some("other-instance")
        );
        assert_eq!(request.targets[0].expected_generation, 42);
        assert_eq!(request.targets[0].target_id, "canonical-target");
        assert_ne!(request.request_id, "");
    }

    #[test]
    fn daemon_without_current_codex_tuning_is_reinstalled_even_at_same_version() {
        assert!(daemon_needs_reinstall(env!("CARGO_PKG_VERSION"), false));
        assert!(daemon_needs_reinstall("older-version", true));
        assert!(!daemon_needs_reinstall(env!("CARGO_PKG_VERSION"), true));
        assert!(!has_required_capabilities(&[
            hsin_ipc::capability::CONTEXT_PRESETS.into(),
        ]));
        assert!(has_required_capabilities(&[
            hsin_ipc::capability::CONTEXT_PRESETS.into(),
            hsin_ipc::capability::PLAN_MODE_REASONING.into(),
            hsin_ipc::capability::CONFIG_OWNERSHIP.into(),
        ]));
        assert!(!has_required_capabilities(&[
            hsin_ipc::capability::CONTEXT_PRESETS.into(),
            hsin_ipc::capability::PLAN_MODE_REASONING.into(),
        ]));
    }

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
