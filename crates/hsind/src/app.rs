use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hsin_core::{
    AppError, AuthScheme, ClientKind, ConnectionMode, DaemonStatus, DoctorFinding, DoctorReport,
    DoctorSeverity, ErrorCode, ImportCurrentParams, KeyStoreState, Provider, ProviderAddParams,
    ProviderEditParams, ProviderListParams, ProviderRemoveParams, ProviderSwitchParams,
    SecretInput, SecurityStatus, Settings, SettingsPatch,
};
use parking_lot::RwLock;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::{
    config::{self, ConfigTarget},
    crypto::{CryptoManager, KeyStore, SystemKeyStore},
    db::Database,
    error::{DaemonError, Result},
    model::ProviderInput,
    paths::Paths,
};

pub struct App {
    pub db: Arc<Database>,
    pub crypto: Arc<CryptoManager>,
    mutation: Mutex<()>,
    credential_command: PathBuf,
    proxy_listening: AtomicBool,
    shutdown_requested: AtomicBool,
    shutdown: tokio::sync::Notify,
    config_paths: RwLock<HashMap<ClientKind, PathBuf>>,
}

enum RecoveryOutcome {
    Complete,
    Aborted,
}

impl App {
    pub fn open(paths: &Paths) -> Result<Arc<Self>> {
        Self::open_with_store(paths, Arc::new(SystemKeyStore::for_home(&paths.home)))
    }

    pub fn open_with_store(paths: &Paths, store: Arc<dyn KeyStore>) -> Result<Arc<Self>> {
        paths.prepare()?;
        let db = Arc::new(Database::open(&paths.database, &paths.backups)?);
        let crypto = Arc::new(CryptoManager::initialize(db.clone(), store)?);
        let credential_command = std::env::current_exe()
            .map_err(DaemonError::Io)?
            .with_file_name("hsin");
        let config_paths = HashMap::from([
            (
                ClientKind::Codex,
                config::default_config_path(ClientKind::Codex)?,
            ),
            (
                ClientKind::Claude,
                config::default_config_path(ClientKind::Claude)?,
            ),
        ]);
        Ok(Arc::new(Self {
            db,
            crypto,
            mutation: Mutex::new(()),
            credential_command,
            proxy_listening: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
            config_paths: RwLock::new(config_paths),
        }))
    }

    pub fn proxy_port(&self) -> Result<u16> {
        self.db
            .setting("proxy_port")?
            .unwrap_or_else(|| "9999".into())
            .parse()
            .map_err(|_| DaemonError::Database(rusqlite::Error::InvalidQuery))
    }

    pub fn mark_proxy_listening(&self, listening: bool) {
        self.proxy_listening.store(listening, Ordering::Release);
    }
    pub fn notify_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.shutdown.notify_waiters();
    }
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }
    pub async fn wait_shutdown(&self) {
        self.shutdown.notified().await;
    }

    pub fn list_providers(&self, params: &ProviderListParams) -> Result<Vec<Provider>> {
        self.db.list_providers(params.client)
    }

    pub async fn add_provider(&self, params: ProviderAddParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let api_key = match params.secret {
            SecretInput::Replace(value) => Some(Zeroizing::new(value)),
            SecretInput::Preserve | SecretInput::Clear => None,
        };
        let input = ProviderInput {
            client: params.provider.client,
            name: params.provider.name,
            base_url: params.provider.base_url,
            auth_scheme: params.provider.auth_scheme,
        };
        let provider = Database::new_provider(&input)?;
        let encrypted = api_key
            .as_deref()
            .map(|secret| self.crypto.encrypt_for(&provider, secret))
            .transpose()?;
        self.db.insert_provider(&provider, encrypted.as_ref())?;
        Ok(provider)
    }

    pub async fn edit_provider(&self, params: ProviderEditParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let current = self.db.get_provider(&params.id)?;
        let input = ProviderInput {
            client: current.client,
            name: params.patch.name.unwrap_or(current.name),
            base_url: params.patch.base_url.unwrap_or(current.base_url),
            auth_scheme: params.patch.auth_scheme.unwrap_or(current.auth_scheme),
        };
        input.validate()?;
        let provider = Provider {
            id: current.id,
            client: current.client,
            name: input.name.trim().to_owned(),
            base_url: input.base_url.trim().trim_end_matches('/').to_owned(),
            auth_scheme: input.auth_scheme,
            revision: current.revision.saturating_add(1),
        };
        let state = self.db.client_state(provider.client)?;
        let active_direct = state.mode == ConnectionMode::Direct
            && state.active_provider_id.as_deref() == Some(provider.id.as_str());
        if active_direct
            && !matches!(
                (provider.client, provider.auth_scheme),
                (ClientKind::Codex, AuthScheme::Bearer) | (ClientKind::Claude, AuthScheme::XApiKey)
            )
        {
            return Err(DaemonError::Invalid(
                "this authentication scheme is supported only through proxy mode for this client"
                    .into(),
            ));
        }
        let encrypted = match params.secret {
            SecretInput::Preserve => None,
            SecretInput::Replace(secret) => Some(self.crypto.encrypt_for(&provider, &secret)?),
            SecretInput::Clear => {
                return Err(DaemonError::Invalid(
                    "clearing a provider credential is not supported; remove the provider instead"
                        .into(),
                ));
            }
        };
        let pending_config = if active_direct {
            let target = ConfigTarget {
                client: provider.client,
                mode: ConnectionMode::Direct,
                provider: provider.clone(),
                credential_command: self.credential_command.to_string_lossy().into_owned(),
                proxy_port: self.proxy_port()?,
            };
            let path = self.config_path(provider.client)?;
            let before_hash = config::file_hash(&path)?;
            let operation = self.db.begin_operation(
                "edit_active_config",
                provider.client,
                before_hash.as_deref(),
                &serde_json::to_string(&target)?,
            )?;
            Some((target, path, before_hash, operation))
        } else {
            None
        };
        if let Err(error) =
            self.db
                .update_provider(&provider, params.expected_revision, encrypted.as_ref())
        {
            if let Some((_, _, _, operation)) = &pending_config {
                self.db
                    .finish_operation(operation, "failed", Some(&error.to_string()))?;
            }
            return Err(error);
        }
        if let Some((target, path, before_hash, operation)) = pending_config {
            if let Err(error) = config::apply(&path, before_hash.as_deref(), &target) {
                let conflict = matches!(error, DaemonError::Conflict(_));
                self.db.finish_operation(
                    &operation,
                    if conflict { "conflict" } else { "failed" },
                    Some(&error.to_string()),
                )?;
                self.db.set_config_status(
                    provider.client,
                    if conflict { "conflict" } else { "unavailable" },
                )?;
                return Err(error);
            }
            self.db
                .set_active(provider.client, &provider.id, "synchronized")?;
            self.db.set_mode(provider.client, ConnectionMode::Direct)?;
            self.db.finish_operation(&operation, "complete", None)?;
        }
        Ok(provider)
    }

    pub async fn remove_provider(&self, params: ProviderRemoveParams) -> Result<()> {
        let _guard = self.mutation.lock().await;
        let provider = self.db.get_provider(&params.id)?;
        if provider.revision != params.expected_revision {
            return Err(DaemonError::Conflict("provider revision changed".into()));
        }
        self.db.remove_provider(&params.id)
    }

    pub async fn import_current(&self, params: ImportCurrentParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let path = self.config_path(params.client)?;
        let text = fs::read_to_string(&path).map_err(|error| {
            DaemonError::Config(format!("cannot read {}: {error}", path.display()))
        })?;
        let (base_url, auth_scheme) = match params.client {
            ClientKind::Codex => {
                let document = text
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| DaemonError::Config(error.to_string()))?;
                let id = document
                    .get("model_provider")
                    .and_then(toml_edit::Item::as_str)
                    .unwrap_or("openai");
                let base_url = document
                    .get("model_providers")
                    .and_then(toml_edit::Item::as_table)
                    .and_then(|providers| providers.get(id))
                    .and_then(toml_edit::Item::as_table)
                    .and_then(|provider| provider.get("base_url"))
                    .and_then(toml_edit::Item::as_str)
                    .unwrap_or("https://api.openai.com/v1")
                    .to_owned();
                (base_url, AuthScheme::Bearer)
            }
            ClientKind::Claude => {
                let value = jsonc_parser::parse_to_serde_value(
                    &text,
                    &jsonc_parser::ParseOptions::default(),
                )
                .map_err(|error| DaemonError::Config(error.to_string()))?
                .ok_or_else(|| DaemonError::Config("empty Claude settings".into()))?;
                let base_url = value
                    .get("env")
                    .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("https://api.anthropic.com")
                    .to_owned();
                (base_url, AuthScheme::XApiKey)
            }
        };
        let input = ProviderInput {
            client: params.client,
            name: params.name,
            base_url,
            auth_scheme,
        };
        self.db.add_provider(&input)
    }

    pub async fn switch_provider(&self, params: ProviderSwitchParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let provider = self.db.get_provider(&params.provider_id)?;
        if provider.client != params.client {
            return Err(DaemonError::Invalid(
                "provider belongs to another client".into(),
            ));
        }
        let _ = self.db.secret(&provider.id)?;
        let state = self.db.client_state(params.client)?;
        if state.mode == ConnectionMode::Proxy {
            self.db
                .set_active(params.client, &provider.id, "synchronized")?;
            return Ok(provider);
        }
        self.apply_configuration(&provider, ConnectionMode::Direct)?;
        Ok(provider)
    }

    pub async fn set_mode(&self, client: ClientKind, mode: ConnectionMode) -> Result<()> {
        let _guard = self.mutation.lock().await;
        let state = self.db.client_state(client)?;
        let id = state
            .active_provider_id
            .ok_or_else(|| DaemonError::NotFound("active provider".into()))?;
        let provider = self.db.get_provider(&id)?;
        self.apply_configuration(&provider, mode)?;
        Ok(())
    }

    fn apply_configuration(&self, provider: &Provider, mode: ConnectionMode) -> Result<()> {
        if mode == ConnectionMode::Direct
            && !matches!(
                (provider.client, provider.auth_scheme),
                (ClientKind::Codex, AuthScheme::Bearer) | (ClientKind::Claude, AuthScheme::XApiKey)
            )
        {
            return Err(DaemonError::Invalid(
                "this authentication scheme is supported only through proxy mode for this client"
                    .into(),
            ));
        }
        let target = ConfigTarget {
            client: provider.client,
            mode,
            provider: provider.clone(),
            credential_command: self.credential_command.to_string_lossy().into_owned(),
            proxy_port: self.proxy_port()?,
        };
        let path = self.config_path(provider.client)?;
        let before_hash = config::file_hash(&path)?;
        let target_json = serde_json::to_string(&target)?;
        let operation = self.db.begin_operation(
            "apply_config",
            provider.client,
            before_hash.as_deref(),
            &target_json,
        )?;
        match config::apply(&path, before_hash.as_deref(), &target) {
            Ok(_) => {
                self.db
                    .set_active(provider.client, &provider.id, "synchronized")?;
                self.db.set_mode(provider.client, mode)?;
                self.db.finish_operation(&operation, "complete", None)?;
                Ok(())
            }
            Err(error) => {
                self.db.finish_operation(
                    &operation,
                    if matches!(error, DaemonError::Conflict(_)) {
                        "conflict"
                    } else {
                        "failed"
                    },
                    Some(&error.to_string()),
                )?;
                self.db.set_config_status(
                    provider.client,
                    if matches!(error, DaemonError::Conflict(_)) {
                        "conflict"
                    } else {
                        "unavailable"
                    },
                )?;
                Err(error)
            }
        }
    }

    pub fn recover_operations(&self) -> Result<()> {
        for (id, client, before_hash, target_json) in self.db.pending_operations()? {
            match self.recover_operation(client, before_hash.as_deref(), &target_json) {
                Ok(RecoveryOutcome::Complete) => {
                    self.db.finish_operation(&id, "complete", None)?;
                }
                Ok(RecoveryOutcome::Aborted) => {
                    self.db.finish_operation(&id, "aborted", None)?;
                }
                Err(error) => {
                    let conflict = matches!(error, DaemonError::Conflict(_));
                    self.db.set_config_status(
                        client,
                        if conflict { "conflict" } else { "unavailable" },
                    )?;
                    self.db.finish_operation(
                        &id,
                        if conflict { "conflict" } else { "failed" },
                        Some(&error.to_string()),
                    )?;
                    tracing::warn!(operation_id = %id, %error, "configuration operation recovery failed");
                }
            }
        }
        Ok(())
    }

    fn recover_operation(
        &self,
        client: ClientKind,
        before_hash: Option<&str>,
        target_json: &str,
    ) -> Result<RecoveryOutcome> {
        let target: ConfigTarget = serde_json::from_str(target_json)?;
        let persisted = self.db.get_provider(&target.provider.id)?;
        if persisted != target.provider {
            if persisted.id == target.provider.id && persisted.revision < target.provider.revision {
                return Ok(RecoveryOutcome::Aborted);
            }
            return Err(DaemonError::Conflict(
                "provider state diverged during recovery".into(),
            ));
        }
        let path = self.config_path(client)?;
        let current = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        let current_hash = config::file_hash(&path)?;
        if current_hash.as_deref() == before_hash {
            config::apply(&path, before_hash, &target)?;
        } else if config::patch_text(&current, &target)? != current {
            return Err(DaemonError::Conflict(
                "configuration diverged during recovery".into(),
            ));
        }
        self.db
            .set_active(client, &target.provider.id, "synchronized")?;
        self.db.set_mode(client, target.mode)?;
        Ok(RecoveryOutcome::Complete)
    }

    pub fn status(&self) -> Result<DaemonStatus> {
        let port = self.proxy_port()?;
        Ok(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            locked: !self.crypto.is_unlocked(),
            proxy_listening: self.proxy_listening.load(Ordering::Acquire),
            proxy_address: format!("127.0.0.1:{port}"),
            clients: vec![
                self.db.client_state(ClientKind::Codex)?,
                self.db.client_state(ClientKind::Claude)?,
            ],
        })
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(Settings {
            language: self
                .db
                .setting("language")?
                .unwrap_or_else(|| "en-US".into()),
            proxy_host: "127.0.0.1".into(),
            proxy_port: self.proxy_port()?,
        })
    }

    pub async fn update_settings(&self, patch: SettingsPatch) -> Result<Settings> {
        let _guard = self.mutation.lock().await;
        if let Some(language) = patch.language {
            if !matches!(language.as_str(), "zh-CN" | "en-US") {
                return Err(DaemonError::Invalid(
                    "language must be zh-CN or en-US".into(),
                ));
            }
            self.db.set_setting("language", &language)?;
        }
        if let Some(port) = patch.proxy_port {
            if port < 1024 {
                return Err(DaemonError::Invalid(
                    "proxy port must be at least 1024".into(),
                ));
            }
            if self.proxy_listening.load(Ordering::Acquire) && port != self.proxy_port()? {
                return Err(DaemonError::Conflict(
                    "restart daemon to change the proxy port".into(),
                ));
            }
            self.db.set_setting("proxy_port", &port.to_string())?;
        }
        self.settings()
    }

    pub fn security_status(&self) -> Result<SecurityStatus> {
        let version = self.db.current_key_record()?.map_or(0, |record| record.0);
        Ok(SecurityStatus {
            key_store: if self.crypto.is_unlocked() {
                KeyStoreState::Unlocked
            } else {
                KeyStoreState::Locked
            },
            key_version: version,
            recovery_key_configured: version > 0,
        })
    }

    pub fn export_recovery_key(&self) -> Result<String> {
        Ok(self
            .crypto
            .export_recovery_key()?
            .expose_secret()
            .to_owned())
    }
    pub fn import_recovery_key(&self, value: &str) -> Result<()> {
        self.crypto.import_recovery_key(value)
    }
    pub async fn rotate_key(&self) -> Result<u32> {
        let _guard = self.mutation.lock().await;
        self.crypto.rotate()
    }

    pub fn credential(
        &self,
        client: ClientKind,
        provider_id: Option<&str>,
        revision: Option<u64>,
    ) -> Result<SecretString> {
        if let Some(provider_id) = provider_id {
            let expected_revision = revision.ok_or_else(|| {
                DaemonError::Invalid("provider-bound credential is missing revision".into())
            })?;
            let (provider, encrypted) =
                self.db
                    .bound_secret(client, provider_id, expected_revision)?;
            return self.crypto.decrypt_for(&provider, &encrypted);
        }
        let state = self.db.client_state(client)?;
        if state.mode == ConnectionMode::Proxy {
            self.proxy_capability(client)
        } else {
            self.crypto.credential_for(client)
        }
    }

    pub fn upstream_snapshot(&self, client: ClientKind) -> Result<(Provider, SecretString)> {
        let (provider, encrypted) = self.db.active_secret(client)?;
        let secret = self.crypto.decrypt_for(&provider, &encrypted)?;
        Ok((provider, secret))
    }

    pub fn proxy_capability(&self, client: ClientKind) -> Result<SecretString> {
        let recovery = self.crypto.export_recovery_key()?;
        let mut digest = Sha256::new();
        digest.update(b"hsin local proxy capability v1\0");
        digest.update(client.as_str().as_bytes());
        digest.update(b"\0");
        digest.update(recovery.expose_secret().as_bytes());
        Ok(SecretString::from(
            URL_SAFE_NO_PAD.encode(digest.finalize()),
        ))
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let mut findings = Vec::new();
        if !self.crypto.is_unlocked() {
            let mut item = finding("key_store_locked", DoctorSeverity::Error);
            if let Some(reason) = self.crypto.lock_reason() {
                item.args.insert("reason".into(), reason);
            }
            findings.push(item);
        }
        if let Err(error) = self.crypto.retry_pending_cleanup() {
            let mut item = finding("key_cleanup_pending", DoctorSeverity::Warning);
            item.args.insert("message".into(), error.to_string());
            findings.push(item);
        }
        if self.db.integrity_check()? != "ok" {
            findings.push(finding("database_integrity_failed", DoctorSeverity::Error));
        }
        if !self.proxy_listening.load(Ordering::Acquire) {
            findings.push(finding("proxy_not_listening", DoctorSeverity::Warning));
        }
        #[cfg(target_os = "linux")]
        if crate::service::uses_fallback() {
            findings.push(finding("systemd_user_unavailable", DoctorSeverity::Warning));
        }
        for client in [ClientKind::Codex, ClientKind::Claude] {
            if !self.config_path(client)?.exists() {
                let mut item = finding("client_config_missing", DoctorSeverity::Info);
                item.args.insert("client".into(), client.to_string());
                findings.push(item);
            }
        }
        Ok(DoctorReport {
            healthy: !findings
                .iter()
                .any(|item| item.severity == DoctorSeverity::Error),
            findings,
        })
    }

    fn config_path(&self, client: ClientKind) -> Result<PathBuf> {
        self.config_paths
            .read()
            .get(&client)
            .cloned()
            .ok_or_else(|| DaemonError::Config("missing client config path".into()))
    }
}

fn finding(code: &str, severity: DoctorSeverity) -> DoctorFinding {
    DoctorFinding {
        code: code.into(),
        severity,
        args: BTreeMap::new(),
    }
}

impl From<&DaemonError> for AppError {
    fn from(error: &DaemonError) -> Self {
        let code = match error {
            DaemonError::Locked => ErrorCode::KeyStoreLocked,
            DaemonError::NotFound(_) => ErrorCode::NotFound,
            DaemonError::Conflict(message)
                if message.contains("revision") || message.contains("changed or was removed") =>
            {
                ErrorCode::RevisionConflict
            }
            DaemonError::Conflict(_) => ErrorCode::ConfigConflict,
            DaemonError::Invalid(_) => ErrorCode::InvalidArgument,
            DaemonError::Config(_) | DaemonError::Io(_) => ErrorCode::ConfigUnavailable,
            DaemonError::Protocol(_) => ErrorCode::ProtocolMismatch,
            DaemonError::Keyring(_) => ErrorCode::KeyStoreUnavailable,
            _ => ErrorCode::Internal,
        };
        AppError::new(code)
            .with_arg("message", error.to_string())
            .retryable(error.retryable())
    }
}
