use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hsin_core::{
    ClientKind, ConnectionMode, DaemonStatus, ErrorCode, Provider, ProviderListParams,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::bootstrap;

#[cfg(feature = "standalone")]
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

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
    backend: Backend,
}

enum Backend {
    Remote(tokio::sync::Mutex<hsin_ipc::IpcClient>),
    #[cfg(feature = "standalone")]
    Embedded(Embedded),
}

/// In-process daemon core for standalone (daemon-less) operation. It owns the
/// same exclusive instance lock as `hsind`, so state ownership stays unique.
/// Proxy mode requires a persistent listener and is rejected here.
#[cfg(feature = "standalone")]
struct Embedded {
    app: Arc<hsind::app::App>,
    _guard: hsind::paths::InstanceGuard,
    next_id: AtomicU64,
}

#[cfg(feature = "standalone")]
impl Embedded {
    fn open() -> Result<Self> {
        let paths = hsind::paths::Paths::discover();
        let store = hsind::crypto::KeyStoreKind::from_env()
            .map_err(|error| daemon_error(&error))?
            .open(&paths.home);
        Self::open_with(&paths, store)
    }

    fn open_with(
        paths: &hsind::paths::Paths,
        store: Arc<dyn hsind::crypto::KeyStore>,
    ) -> Result<Self> {
        paths.prepare().map_err(|error| daemon_error(&error))?;
        let guard = match hsind::paths::InstanceGuard::acquire(&paths.lock) {
            Ok(guard) => guard,
            Err(hsind::error::DaemonError::Conflict(_)) => {
                return Err(anyhow!(
                    "another hsin process owns the shared state, but no daemon IPC endpoint is \
                     reachable; standalone mode cannot start"
                ));
            }
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context("acquire the standalone state-owner lock")
                );
            }
        };
        let app = hsind::app::App::open_standalone_with_store(paths, store)
            .map_err(|error| daemon_error(&error))?;
        app.recover_operations()
            .map_err(|error| daemon_error(&error))?;
        app.initialize_providers()
            .map_err(|error| daemon_error(&error))?;
        app.reconcile_client_auth_configuration()
            .map_err(|error| daemon_error(&error))?;
        Ok(Self {
            app,
            _guard: guard,
            next_id: AtomicU64::new(1),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        hsind::rpc::dispatch(self.app.clone(), id, method, params)
            .await
            .into_result()
            .map_err(|error| anyhow::Error::new(hsin_ipc::TransportError::Rpc(error)))
    }
}

#[cfg(feature = "standalone")]
fn daemon_error(error: &hsind::error::DaemonError) -> anyhow::Error {
    anyhow::Error::new(hsin_ipc::TransportError::Rpc(
        hsin_ipc::RpcError::application(hsin_core::AppError::from(error)),
    ))
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
            backend: Backend::Remote(tokio::sync::Mutex::new(inner)),
        })
    }

    /// Open the daemon core in-process, without a running `hsind`.
    #[cfg(feature = "standalone")]
    pub fn open_standalone() -> Result<Self> {
        Ok(Self {
            backend: Backend::Embedded(Embedded::open()?),
        })
    }

    /// Report that this build does not contain the optional embedded daemon core.
    #[cfg(not(feature = "standalone"))]
    pub fn open_standalone() -> Result<Self> {
        Err(anyhow!(
            "this hsin build does not include standalone support; install hsind or rebuild with \
             the standalone feature"
        ))
    }

    pub async fn connect_or_bootstrap(no_daemon: bool) -> Result<Self> {
        if no_daemon {
            return Self::open_standalone().context("standalone mode was requested");
        }

        let initial_error = match Self::connect().await {
            Ok(client) => return Ok(client),
            Err(error) => error,
        };

        // A deployment that never shipped hsind falls back to the embedded core.
        if !bootstrap::daemon_available() {
            return Self::open_standalone().with_context(|| {
                format!("hsind is unavailable ({initial_error:#}) and standalone mode failed")
            });
        }

        if requires_reinstall(&initial_error) {
            bootstrap::install_and_start().await?;
            return Self::wait_until_ready(Some(initial_error)).await;
        }

        // Service status follows the shared instance lock, including an
        // embedded owner, so this branch must never launch a second daemon.
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
        Err(last_error.context("hsind service is running but IPC did not become ready"))
    }

    async fn wait_until_ready(mut last_error: Option<anyhow::Error>) -> Result<Self> {
        for _ in 0..DAEMON_READY_RETRIES {
            tokio::time::sleep(DAEMON_READY_RETRY_DELAY).await;
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("daemon did not become ready")))
    }

    pub async fn call<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        match &self.backend {
            Backend::Remote(inner) => inner
                .lock()
                .await
                .call(method, params)
                .await
                .with_context(|| format!("RPC {method} failed")),
            #[cfg(feature = "standalone")]
            Backend::Embedded(embedded) => {
                let params = serde_json::to_value(params)
                    .with_context(|| format!("encode {method} parameters"))?;
                let value = embedded
                    .call(method, params)
                    .await
                    .with_context(|| format!("RPC {method} failed"))?;
                serde_json::from_value(value).with_context(|| format!("decode {method} response"))
            }
        }
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

    #[cfg(feature = "standalone")]
    #[derive(Default)]
    struct MemoryStore(std::sync::Mutex<std::collections::HashMap<u32, String>>);

    #[cfg(feature = "standalone")]
    impl hsind::crypto::KeyStore for MemoryStore {
        fn load(&self, version: u32) -> hsind::error::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(&version).cloned())
        }

        fn store(&self, version: u32, value: &str) -> hsind::error::Result<()> {
            self.0.lock().unwrap().insert(version, value.to_owned());
            Ok(())
        }

        fn delete(&self, version: u32) -> hsind::error::Result<()> {
            self.0.lock().unwrap().remove(&version);
            Ok(())
        }
    }

    #[cfg(feature = "standalone")]
    fn standalone_paths(label: &str) -> hsind::paths::Paths {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let home = std::env::temp_dir().join(format!(
            "hsin-standalone-{label}-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        hsind::paths::Paths {
            database: home.join("hsin.sqlite3"),
            lock: home.join("hsind.lock"),
            logs: home.join("logs"),
            backups: home.join("backups"),
            home,
        }
    }

    #[cfg(feature = "standalone")]
    fn seed_standalone_database(
        paths: &hsind::paths::Paths,
        configure: impl FnOnce(&hsind::db::Database),
    ) {
        std::fs::create_dir_all(&paths.home).unwrap();
        let db = hsind::db::Database::open(&paths.database, &paths.backups).unwrap();
        db.set_setting("providers_initialized_v1", "true").unwrap();
        configure(&db);
    }

    #[cfg(feature = "standalone")]
    fn application_error_code(error: &anyhow::Error) -> ErrorCode {
        error
            .chain()
            .find_map(|source| {
                let hsin_ipc::TransportError::Rpc(rpc) =
                    source.downcast_ref::<hsin_ipc::TransportError>()?
                else {
                    return None;
                };
                rpc.data.as_ref().map(|application| application.code)
            })
            .expect("application error payload")
    }

    #[cfg(feature = "standalone")]
    #[tokio::test]
    async fn embedded_dispatch_rejects_proxy_operations_with_a_stable_code() {
        let paths = standalone_paths("dispatch");
        seed_standalone_database(&paths, |_| {});
        let embedded = Embedded::open_with(&paths, Arc::new(MemoryStore::default())).unwrap();

        let status = embedded.call("status", json!({})).await.unwrap();
        assert_eq!(status["proxy_enabled"], false);

        let error = embedded
            .call("mode.set", json!({"client": "codex", "mode": "proxy"}))
            .await
            .unwrap_err();
        assert_eq!(
            application_error_code(&error),
            ErrorCode::ProxyRequiresDaemon
        );
        let error = embedded
            .call("settings.set", json!({"proxy_enabled": true}))
            .await
            .unwrap_err();
        assert_eq!(
            application_error_code(&error),
            ErrorCode::ProxyRequiresDaemon
        );

        drop(embedded);
        std::fs::remove_dir_all(paths.home).unwrap();
    }

    #[cfg(feature = "standalone")]
    #[test]
    fn embedded_open_rejects_persisted_proxy_state() {
        type ProxyStateCase = (&'static str, fn(&hsind::db::Database));
        let cases: [ProxyStateCase; 2] = [
            ("client-mode", |db: &hsind::db::Database| {
                db.set_mode(ClientKind::Codex, ConnectionMode::Proxy)
                    .unwrap();
            }),
            ("listener-enabled", |db: &hsind::db::Database| {
                db.set_setting("proxy_enabled", "true").unwrap();
            }),
        ];
        for (label, configure) in cases {
            let paths = standalone_paths(label);
            seed_standalone_database(&paths, configure);
            let error = Embedded::open_with(&paths, Arc::new(MemoryStore::default()))
                .err()
                .expect("persisted proxy state must be rejected");
            assert_eq!(
                application_error_code(&error),
                ErrorCode::ProxyRequiresDaemon
            );
            std::fs::remove_dir_all(paths.home).unwrap();
        }
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
