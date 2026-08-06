use std::{
    collections::{BTreeMap, HashMap},
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hsin_core::{
    AppError, AuthScheme, ClaudeModelMappingUpdate, ClientAuthSettings, ClientKind, ClientSettings,
    ConnectionMode, DaemonStatus, DoctorFinding, DoctorReport, DoctorSeverity, ErrorCode,
    ImportCurrentParams, ImportCurrentResult, KeyStoreState, ModelDiscoverParams, ModelDiscovery,
    ModelUpdate, Provider, ProviderAddParams, ProviderEditParams, ProviderListParams,
    ProviderRemoveParams, ProviderSwitchParams, SecretInput, SecurityStatus, Settings,
    SettingsPatch,
};
use parking_lot::RwLock;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, watch};
use zeroize::Zeroizing;

use crate::{
    config::{self, ConfigTarget},
    crypto::{self, CryptoManager, KeyStore},
    db::Database,
    error::{DaemonError, Result},
    model::ProviderInput,
    paths::Paths,
};

const CODEX_AUTH_BACKUP_KEY: &str = "codex_auth_backup_v1";
/// Records that the operator holds a copy of the current master key. Losing the
/// keyring entry without one is unrecoverable, so this must track the actual
/// export instead of merely asserting that a key exists.
const RECOVERY_KEY_EXPORTED_KEY: &str = "recovery_key_exported";

fn client_host_for_listener(host: IpAddr) -> IpAddr {
    match host {
        IpAddr::V4(host) if host.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(host) if host.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        host => host,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProxyRuntimeConfig {
    pub enabled: bool,
    pub address: SocketAddr,
    pub revision: u64,
}

pub struct App {
    pub db: Arc<Database>,
    pub crypto: Arc<CryptoManager>,
    mutation: Mutex<()>,
    credential_command: PathBuf,
    proxy_listening: AtomicBool,
    proxy_active_address: RwLock<Option<SocketAddr>>,
    proxy_failure: RwLock<Option<String>>,
    proxy_runtime: watch::Sender<ProxyRuntimeConfig>,
    shutdown_requested: AtomicBool,
    shutdown: tokio::sync::Notify,
    config_paths: RwLock<HashMap<ClientKind, PathBuf>>,
    http_client: reqwest::Client,
}

enum RecoveryOutcome {
    Complete,
    Aborted,
}

fn credential_preview(secret: &str) -> String {
    let characters = secret.chars().collect::<Vec<_>>();
    if characters.len() < 12 {
        return "••••••••".into();
    }
    let prefix = if secret.starts_with("sk-") { 6 } else { 4 };
    let visible_prefix = characters.iter().take(prefix).collect::<String>();
    let visible_suffix = characters.iter().rev().take(2).rev().collect::<String>();
    format!("{visible_prefix}***{visible_suffix}")
}

impl App {
    pub fn open(paths: &Paths) -> Result<Arc<Self>> {
        Self::open_with_store(paths, crypto::default_key_store(&paths.home))
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
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(4))
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .map_err(|error| DaemonError::Internal(error.to_string()))?;
        let enabled = db
            .setting("proxy_enabled")?
            .is_some_and(|value| value == "true");
        let proxy_host = db
            .setting("proxy_host")?
            .unwrap_or_else(|| "127.0.0.1".into())
            .parse::<IpAddr>()
            .map_err(|_| DaemonError::Config("invalid stored proxy host".into()))?;
        let proxy_port = db
            .setting("proxy_port")?
            .unwrap_or_else(|| "9999".into())
            .parse::<u16>()
            .map_err(|_| DaemonError::Config("invalid stored proxy port".into()))?;
        let (proxy_runtime, _) = watch::channel(ProxyRuntimeConfig {
            enabled,
            address: SocketAddr::new(proxy_host, proxy_port),
            revision: 0,
        });
        Ok(Arc::new(Self {
            db,
            crypto,
            mutation: Mutex::new(()),
            credential_command,
            proxy_listening: AtomicBool::new(false),
            proxy_active_address: RwLock::new(None),
            proxy_failure: RwLock::new(None),
            proxy_runtime,
            shutdown_requested: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
            config_paths: RwLock::new(config_paths),
            http_client,
        }))
    }

    pub fn proxy_port(&self) -> Result<u16> {
        self.db
            .setting("proxy_port")?
            .unwrap_or_else(|| "9999".into())
            .parse()
            .map_err(|_| DaemonError::Database(rusqlite::Error::InvalidQuery))
    }

    pub fn proxy_host(&self) -> Result<IpAddr> {
        self.db
            .setting("proxy_host")?
            .unwrap_or_else(|| "127.0.0.1".into())
            .parse()
            .map_err(|_| DaemonError::Config("invalid stored proxy host".into()))
    }
    pub fn proxy_address(&self) -> Result<SocketAddr> {
        Ok(SocketAddr::new(self.proxy_host()?, self.proxy_port()?))
    }
    fn proxy_client_host(&self) -> Result<IpAddr> {
        Ok(client_host_for_listener(self.proxy_host()?))
    }
    pub fn mark_proxy_active(&self, address: Option<SocketAddr>) {
        *self.proxy_active_address.write() = address;
        self.proxy_listening
            .store(address.is_some(), Ordering::Release);
        if address.is_some() {
            *self.proxy_failure.write() = None;
        }
    }
    /// Record why the listener could not come up. The readiness wait only sees
    /// a timeout, and the real cause -- almost always a taken port -- would
    /// otherwise be visible in the service log only.
    pub fn mark_proxy_failure(&self, reason: &str) {
        *self.proxy_failure.write() = Some(reason.to_owned());
    }
    pub fn proxy_enabled(&self) -> bool {
        self.proxy_runtime.borrow().enabled
    }
    pub fn subscribe_proxy_runtime(&self) -> watch::Receiver<ProxyRuntimeConfig> {
        self.proxy_runtime.subscribe()
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

    pub(crate) fn initialize_providers(&self) -> Result<()> {
        if self.db.setting("providers_initialized_v1")?.as_deref() == Some("true") {
            return Ok(());
        }
        for client in [ClientKind::Codex, ClientKind::Claude] {
            let official = self.ensure_official_provider(client)?;
            let detected = match config::detect_current(&self.config_path(client)?, client) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!(%client, %error, "could not import the current provider; using Official metadata");
                    self.db.set_active(client, &official.id, "unmanaged")?;
                    self.db.set_mode(client, ConnectionMode::Direct)?;
                    continue;
                }
            };
            if detected.official {
                self.db.set_active(client, &official.id, "synchronized")?;
                self.db.set_mode(client, ConnectionMode::Direct)?;
                continue;
            }
            if let Some(provider) = self.current_configuration_provider(client)? {
                self.db.set_active(client, &provider.id, "synchronized")?;
                continue;
            }
            let imported = self
                .matching_detected_provider(client, &detected)?
                .map_or_else(|| self.insert_detected_provider(client, detected), Ok)?;
            self.db.set_active(client, &imported.id, "synchronized")?;
            self.db.set_mode(client, ConnectionMode::Direct)?;
        }
        self.db.set_setting("providers_initialized_v1", "true")
    }

    pub(crate) fn reconcile_client_auth_configuration(&self) -> Result<()> {
        let state = self.db.client_state(ClientKind::Codex)?;
        let Some(provider_id) = state.active_provider_id else {
            return Ok(());
        };
        let provider = self.db.get_provider(&provider_id)?;
        if provider.official && self.codex_auth_backup()?.is_none() {
            return Ok(());
        }
        self.apply_configuration_with_auth(
            &provider,
            state.mode,
            Some(self.disable_custom_auth(ClientKind::Codex)?),
        )
    }

    fn ensure_official_provider(&self, client: ClientKind) -> Result<Provider> {
        let id = format!("official-{}", client.as_str());
        match self.db.get_provider(&id) {
            Ok(provider) => return Ok(provider),
            Err(DaemonError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let detected = config::official_provider(client);
        let mut name = detected.name;
        let names = self.db.list_providers(Some(client))?;
        if names
            .iter()
            .any(|provider| provider.name.eq_ignore_ascii_case(&name))
        {
            name = "Official (hsin)".into();
        }
        let provider = Provider {
            id,
            client,
            name,
            description: String::new(),
            base_url: detected.base_url,
            auth_scheme: AuthScheme::OAuth,
            official: true,
            credential_configured: false,
            credential_preview: None,
            model: None,
            claude_model_mapping: None,
            revision: 1,
        };
        self.db.insert_provider(&provider, None)?;
        Ok(provider)
    }

    fn insert_detected_provider(
        &self,
        client: ClientKind,
        detected: config::DetectedProvider,
    ) -> Result<Provider> {
        let existing = self.db.list_providers(Some(client))?;
        let mut name = detected.name.trim().to_owned();
        if name.is_empty() || name.eq_ignore_ascii_case("Official") {
            name = "Imported Provider".into();
        }
        let base_name = name.clone();
        let mut suffix = 1_u32;
        while existing
            .iter()
            .any(|provider| provider.name.eq_ignore_ascii_case(&name))
        {
            suffix = suffix.saturating_add(1);
            name = format!("{base_name} (Imported {suffix})");
        }
        let input = ProviderInput {
            client,
            name,
            description: String::new(),
            base_url: detected.base_url,
            auth_scheme: detected.auth_scheme,
            model: None,
            claude_model_mapping: None,
        };
        let mut provider = Database::new_provider(&input)?;
        let encrypted = detected
            .secret
            .as_ref()
            .filter(|_| self.crypto.is_unlocked())
            .map(|secret| self.crypto.encrypt_for(&provider, secret))
            .transpose()?;
        provider.credential_configured = encrypted.is_some();
        self.db.insert_provider(&provider, encrypted.as_ref())?;
        Ok(provider)
    }

    fn matching_detected_provider(
        &self,
        client: ClientKind,
        detected: &config::DetectedProvider,
    ) -> Result<Option<Provider>> {
        if detected.official {
            return self.ensure_official_provider(client).map(Some);
        }
        let Some(detected_secret) = detected.secret.as_ref() else {
            return Ok(None);
        };
        for provider in self.db.list_providers(Some(client))? {
            if provider.official
                || provider.auth_scheme != detected.auth_scheme
                || !provider
                    .base_url
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(detected.base_url.trim_end_matches('/'))
                || !provider.credential_configured
            {
                continue;
            }
            let encrypted = self.db.secret(&provider.id)?;
            let stored = self.crypto.decrypt_for(&provider, &encrypted)?;
            if bool::from(
                stored
                    .expose_secret()
                    .as_bytes()
                    .ct_eq(detected_secret.as_bytes()),
            ) {
                return Ok(Some(provider));
            }
        }
        Ok(None)
    }

    fn current_configuration_provider(&self, client: ClientKind) -> Result<Option<Provider>> {
        let state = self.db.client_state(client)?;
        let Some(provider_id) = state.active_provider_id else {
            return Ok(None);
        };
        let provider = self.db.get_provider(&provider_id)?;
        let path = self.config_path(client)?;
        let current = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        let target = self.config_target(&provider, state.mode, None)?;
        let credential = self.config_credential(&target)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        let config_matches =
            config::patch_text_with_credential(&current, &target, credential)? == current;
        let auth_matches = self.codex_auth_target_is_applied(&target, credential)?;
        Ok((config_matches && auth_matches).then_some(provider))
    }

    pub fn list_providers(&self, params: &ProviderListParams) -> Result<Vec<Provider>> {
        let mut providers = self.db.list_providers(params.client)?;
        for provider in &mut providers {
            if provider.official || !provider.credential_configured {
                continue;
            }
            provider.credential_preview = self
                .db
                .secret(&provider.id)
                .and_then(|encrypted| self.crypto.decrypt_for(provider, &encrypted))
                .ok()
                .map(|secret| credential_preview(secret.expose_secret()));
        }
        Ok(providers)
    }

    pub async fn add_provider(&self, params: ProviderAddParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        if params.provider.auth_scheme == AuthScheme::OAuth {
            return Err(DaemonError::Invalid(
                "OAuth is reserved for Official providers".into(),
            ));
        }
        let api_key = match params.secret {
            SecretInput::Replace(value) if !value.trim().is_empty() => Zeroizing::new(value),
            SecretInput::Replace(_) | SecretInput::Preserve | SecretInput::Clear => {
                return Err(DaemonError::Invalid("provider API key is required".into()));
            }
        };
        let input = ProviderInput {
            client: params.provider.client,
            name: params.provider.name,
            description: params.provider.description,
            base_url: params.provider.base_url,
            auth_scheme: params.provider.auth_scheme,
            model: params.provider.model,
            claude_model_mapping: params.provider.claude_model_mapping,
        };
        let mut provider = Database::new_provider(&input)?;
        let encrypted = self.crypto.encrypt_for(&provider, &api_key)?;
        provider.credential_configured = true;
        self.db.insert_provider(&provider, Some(&encrypted))?;
        Ok(provider)
    }

    #[allow(clippy::too_many_lines)]
    pub async fn edit_provider(&self, params: ProviderEditParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let current = self.db.get_provider(&params.id)?;
        if current.official {
            return Err(DaemonError::Invalid(
                "Official providers cannot be edited".into(),
            ));
        }
        let model = match params.patch.model {
            ModelUpdate::Preserve => current.model.clone(),
            ModelUpdate::Set(model) => Some(model),
            ModelUpdate::Clear => None,
        };
        let claude_model_mapping = match params.patch.claude_model_mapping {
            ClaudeModelMappingUpdate::Preserve => current.claude_model_mapping.clone(),
            ClaudeModelMappingUpdate::Set(mapping) => Some(mapping),
            ClaudeModelMappingUpdate::Clear => None,
        };
        let name = params.patch.name.unwrap_or(current.name);
        let input = ProviderInput {
            client: current.client,
            name,
            description: params.patch.description.unwrap_or(current.description),
            base_url: params.patch.base_url.unwrap_or(current.base_url),
            auth_scheme: params.patch.auth_scheme.unwrap_or(current.auth_scheme),
            model,
            claude_model_mapping,
        };
        input.validate()?;
        let mut provider = Provider {
            id: current.id,
            client: current.client,
            name: input.normalized_name()?,
            description: input.description.trim().to_owned(),
            base_url: input.base_url.trim().trim_end_matches('/').to_owned(),
            auth_scheme: input.auth_scheme,
            official: false,
            credential_configured: current.credential_configured,
            credential_preview: None,
            model: input.model.as_ref().map(|model| model.trim().to_owned()),
            claude_model_mapping: input.claude_model_mapping.clone(),
            revision: current.revision.saturating_add(1),
        };
        let state = self.db.client_state(provider.client)?;
        let active = state.active_provider_id.as_deref() == Some(provider.id.as_str());
        let active_direct = active && state.mode == ConnectionMode::Direct;
        let update_active_config = active
            && (active_direct
                || (provider.client == ClientKind::Codex && provider.model.is_some()));
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
            SecretInput::Replace(secret) if !secret.trim().is_empty() => {
                Some(self.crypto.encrypt_for(&provider, &secret)?)
            }
            SecretInput::Replace(_) => {
                return Err(DaemonError::Invalid(
                    "provider API key cannot be empty".into(),
                ));
            }
            SecretInput::Clear => {
                return Err(DaemonError::Invalid(
                    "clearing a provider credential is not supported; remove the provider instead"
                        .into(),
                ));
            }
        };
        if encrypted.is_some() {
            provider.credential_configured = true;
        }
        let pending_config = if update_active_config {
            let target = self.config_target(&provider, state.mode, None)?;
            let path = self.config_path(provider.client)?;
            let before_hash = config::file_hash(&path)?;
            let operation = self.db.begin_operation(
                "edit_active_config",
                provider.client,
                before_hash.as_deref(),
                &serde_json::to_string(&target)?,
            )?;
            let backup_created = if Self::manages_codex_auth(&target) {
                let auth_path = config::codex_auth_path(&path)?;
                match self.ensure_codex_auth_backup(&auth_path) {
                    Ok(created) => created,
                    Err(error) => {
                        self.db
                            .finish_operation(&operation, "failed", Some(&error.to_string()))?;
                        return Err(error);
                    }
                }
            } else {
                false
            };
            Some((target, path, before_hash, operation, backup_created))
        } else {
            None
        };
        if let Err(error) =
            self.db
                .update_provider(&provider, params.expected_revision, encrypted.as_ref())
        {
            if let Some((_, _, _, operation, backup_created)) = &pending_config {
                if *backup_created {
                    self.remove_codex_auth_backup()?;
                }
                self.db
                    .finish_operation(operation, "failed", Some(&error.to_string()))?;
            }
            return Err(error);
        }
        if let Some((target, path, before_hash, operation, backup_created)) = pending_config {
            let credential = self.config_credential(&target)?;
            if let Err(error) = config::apply_with_credential(
                &path,
                before_hash.as_deref(),
                &target,
                credential.as_ref().map(ExposeSecret::expose_secret),
            ) {
                if backup_created {
                    self.remove_codex_auth_backup()?;
                }
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
            if let Err(error) = self.apply_codex_auth_target(
                &target,
                credential.as_ref().map(ExposeSecret::expose_secret),
            ) {
                self.db.set_config_status(provider.client, "unavailable")?;
                return Err(error);
            }
            if target.client == ClientKind::Codex && !Self::manages_codex_auth(&target) {
                self.remove_codex_auth_backup()?;
            }
            self.db
                .set_active(provider.client, &provider.id, "synchronized")?;
            self.db.set_mode(provider.client, target.mode)?;
            self.db.finish_operation(&operation, "complete", None)?;
        }
        Ok(provider)
    }

    pub async fn discover_models(&self, params: ModelDiscoverParams) -> Result<ModelDiscovery> {
        if params.client != ClientKind::Codex {
            return Err(DaemonError::Invalid(
                "model discovery is currently supported only for Codex providers".into(),
            ));
        }
        let input = ProviderInput {
            client: params.client,
            name: "model-discovery".into(),
            description: String::new(),
            base_url: params.base_url.clone(),
            auth_scheme: params.auth_scheme,
            model: None,
            claude_model_mapping: None,
        };
        input.validate()?;
        let secret = match params.secret {
            SecretInput::Replace(value) if !value.trim().is_empty() => SecretString::from(value),
            SecretInput::Preserve => {
                let provider_id = params.provider_id.ok_or_else(|| {
                    DaemonError::Invalid("provider ID is required to preserve the API key".into())
                })?;
                let provider = self.db.get_provider(&provider_id)?;
                if provider.client != params.client {
                    return Err(DaemonError::Invalid(
                        "provider belongs to a different client".into(),
                    ));
                }
                let encrypted = self.db.secret(&provider_id)?;
                self.crypto.decrypt_for(&provider, &encrypted)?
            }
            SecretInput::Replace(_) | SecretInput::Clear => {
                return Err(DaemonError::Invalid("provider API key is required".into()));
            }
        };

        let original = params.base_url.trim().trim_end_matches('/').to_owned();
        match self
            .fetch_models(&original, params.auth_scheme, &secret)
            .await
        {
            Ok(models) => Ok(ModelDiscovery {
                models,
                resolved_base_url: original,
            }),
            Err(first_error) if !base_url_has_v1(&original) => {
                let with_v1 = format!("{original}/v1");
                self.fetch_models(&with_v1, params.auth_scheme, &secret)
                    .await
                    .map(|models| ModelDiscovery {
                        models,
                        resolved_base_url: with_v1,
                    })
                    .map_err(|second_error| {
                        DaemonError::Config(format!(
                            "model discovery failed at the configured URL ({first_error}) and /v1 fallback ({second_error})"
                        ))
                    })
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch_models(
        &self,
        base_url: &str,
        auth_scheme: AuthScheme,
        secret: &SecretString,
    ) -> Result<Vec<String>> {
        let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
        let mut request = self.http_client.get(endpoint);
        request = match auth_scheme {
            AuthScheme::Bearer => request.bearer_auth(secret.expose_secret()),
            AuthScheme::XApiKey => request.header("x-api-key", secret.expose_secret()),
            AuthScheme::OAuth => {
                return Err(DaemonError::Invalid(
                    "Official OAuth credentials are managed by the client".into(),
                ));
            }
        };
        let response = request
            .send()
            .await
            .map_err(|error| DaemonError::Config(format!("model request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DaemonError::Config(format!(
                "model endpoint returned HTTP {status}"
            )));
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|error| DaemonError::Config(format!("invalid model response: {error}")))?;
        parse_model_ids(&value)
    }

    pub async fn remove_provider(&self, params: ProviderRemoveParams) -> Result<()> {
        let _guard = self.mutation.lock().await;
        let provider = self.db.get_provider(&params.id)?;
        if provider.official {
            return Err(DaemonError::Invalid(
                "Official providers cannot be removed".into(),
            ));
        }
        if provider.revision != params.expected_revision {
            return Err(DaemonError::Conflict("provider revision changed".into()));
        }
        self.db.remove_provider(&params.id)
    }

    pub async fn import_current(&self, params: ImportCurrentParams) -> Result<ImportCurrentResult> {
        let _guard = self.mutation.lock().await;
        if let Some(provider) = self.current_configuration_provider(params.client)? {
            return Ok(ImportCurrentResult {
                provider,
                imported: false,
            });
        }
        let path = self.config_path(params.client)?;
        let mut detected = config::detect_current(&path, params.client)?;
        if detected.official {
            let provider = self.ensure_official_provider(params.client)?;
            self.import_official_provider(&provider)?;
            return Ok(ImportCurrentResult {
                provider,
                imported: false,
            });
        }
        if !params.name.trim().is_empty() {
            detected.name = params.name;
        }
        if detected.secret.is_none() {
            return Err(DaemonError::CurrentCredentialUnavailable);
        }
        let (provider, imported) =
            match self.matching_detected_provider(params.client, &detected)? {
                Some(provider) => (provider, false),
                None => (
                    self.insert_detected_provider(params.client, detected)?,
                    true,
                ),
            };
        self.db
            .set_active(params.client, &provider.id, "synchronized")?;
        self.db.set_mode(params.client, ConnectionMode::Direct)?;
        Ok(ImportCurrentResult { provider, imported })
    }

    fn import_official_provider(&self, provider: &Provider) -> Result<()> {
        if provider.client != ClientKind::Codex || self.codex_auth_backup()?.is_none() {
            self.db
                .set_active(provider.client, &provider.id, "synchronized")?;
            self.db.set_mode(provider.client, ConnectionMode::Direct)?;
            return Ok(());
        }
        let target = self.config_target(provider, ConnectionMode::Direct, None)?;
        let before_hash = target.codex_auth_before_hash.clone();
        let target_json = serde_json::to_string(&target)?;
        let operation = self.db.begin_operation(
            "import_official_auth",
            provider.client,
            before_hash.as_deref(),
            &target_json,
        )?;
        if let Err(error) = self.recover_official_import_operation(
            provider.client,
            before_hash.as_deref(),
            &target_json,
        ) {
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
        self.db.finish_operation(&operation, "complete", None)
    }

    pub async fn switch_provider(&self, params: ProviderSwitchParams) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let provider = self.db.get_provider(&params.provider_id)?;
        if provider.client != params.client {
            return Err(DaemonError::Invalid(
                "provider belongs to another client".into(),
            ));
        }
        if provider.official {
            self.apply_configuration(&provider, ConnectionMode::Direct)?;
            return Ok(provider);
        }
        let _ = self.db.secret(&provider.id)?;
        let state = self.db.client_state(params.client)?;
        if state.mode == ConnectionMode::Proxy {
            if provider.client == ClientKind::Codex && provider.model.is_some() {
                self.apply_configuration(&provider, ConnectionMode::Proxy)?;
                return Ok(provider);
            }
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
            .ok_or(DaemonError::NoActiveProvider)?;
        let provider = self.db.get_provider(&id)?;
        if mode == ConnectionMode::Proxy {
            if provider.auth_scheme == AuthScheme::OAuth {
                return Err(DaemonError::OAuthProxyUnsupported);
            }
            self.set_proxy_enabled_locked(true).await?;
        }
        self.apply_configuration(&provider, mode)?;
        Ok(())
    }

    fn apply_configuration(&self, provider: &Provider, mode: ConnectionMode) -> Result<()> {
        self.apply_configuration_with_auth(provider, mode, None)
    }

    fn apply_configuration_with_auth(
        &self,
        provider: &Provider,
        mode: ConnectionMode,
        disable_custom_auth: Option<bool>,
    ) -> Result<()> {
        if mode == ConnectionMode::Proxy && provider.auth_scheme == AuthScheme::OAuth {
            return Err(DaemonError::OAuthProxyUnsupported);
        }
        if mode == ConnectionMode::Direct
            && !matches!(
                (provider.client, provider.auth_scheme),
                (ClientKind::Codex, AuthScheme::Bearer | AuthScheme::OAuth)
                    | (ClientKind::Claude, AuthScheme::XApiKey | AuthScheme::OAuth)
            )
        {
            return Err(DaemonError::Invalid(
                "this authentication scheme is supported only through proxy mode for this client"
                    .into(),
            ));
        }
        let target = self.config_target(provider, mode, disable_custom_auth)?;
        let credential = self.config_credential(&target)?;
        let path = self.config_path(provider.client)?;
        let before_hash = config::file_hash(&path)?;
        let target_json = serde_json::to_string(&target)?;
        let operation = self.db.begin_operation(
            "apply_config",
            provider.client,
            before_hash.as_deref(),
            &target_json,
        )?;
        let backup_created = if Self::manages_codex_auth(&target) {
            let auth_path = config::codex_auth_path(&path)?;
            match self.ensure_codex_auth_backup(&auth_path) {
                Ok(created) => created,
                Err(error) => {
                    self.db
                        .finish_operation(&operation, "failed", Some(&error.to_string()))?;
                    return Err(error);
                }
            }
        } else {
            false
        };
        if let Err(error) = config::apply_with_credential(
            &path,
            before_hash.as_deref(),
            &target,
            credential.as_ref().map(ExposeSecret::expose_secret),
        ) {
            if backup_created {
                self.remove_codex_auth_backup()?;
            }
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
            return Err(error);
        }
        if let Err(error) = self.apply_codex_auth_target(
            &target,
            credential.as_ref().map(ExposeSecret::expose_secret),
        ) {
            self.db.set_config_status(provider.client, "unavailable")?;
            return Err(error);
        }
        if target.client == ClientKind::Codex && !Self::manages_codex_auth(&target) {
            self.remove_codex_auth_backup()?;
        }
        self.db
            .set_active(provider.client, &provider.id, "synchronized")?;
        self.db.set_mode(provider.client, mode)?;
        if let Some(disabled) = disable_custom_auth {
            let mut client_auth = self.client_auth_settings()?;
            client_auth.set_disable_custom_auth(provider.client, disabled);
            self.db
                .set_setting("client_auth", &serde_json::to_string(&client_auth)?)?;
        }
        self.db.finish_operation(&operation, "complete", None)?;
        Ok(())
    }

    fn config_target(
        &self,
        provider: &Provider,
        mode: ConnectionMode,
        disable_custom_auth: Option<bool>,
    ) -> Result<ConfigTarget> {
        let codex_auth_before_hash = if provider.client == ClientKind::Codex {
            let config_path = self.config_path(ClientKind::Codex)?;
            config::file_hash(&config::codex_auth_path(&config_path)?)?
        } else {
            None
        };
        let claude_model_env_before = if provider.client == ClientKind::Claude {
            Some(self.claude_model_env_snapshot()?)
        } else {
            None
        };
        Ok(ConfigTarget {
            client: provider.client,
            mode,
            provider: provider.clone(),
            credential_command: self.credential_command.to_string_lossy().into_owned(),
            proxy_host: self.proxy_client_host()?.to_string(),
            proxy_port: self.proxy_port()?,
            disable_custom_auth: disable_custom_auth
                .unwrap_or(self.disable_custom_auth(provider.client)?),
            codex_auth_before_hash,
            claude_model_env_before,
        })
    }

    /// The user's own values for the model-mapping env keys, captured the first time hsin builds a
    /// Claude config target and reused from then on — after that first capture the file may hold
    /// hsin's own values, which must never be mistaken for the user's.
    fn claude_model_env_snapshot(&self) -> Result<config::ClaudeModelEnvSnapshot> {
        const SETTING: &str = "claude_model_env_before";
        if let Some(stored) = self.db.setting(SETTING)? {
            return Ok(serde_json::from_str(&stored)?);
        }
        let path = self.config_path(ClientKind::Claude)?;
        let text = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        let snapshot = config::ClaudeModelEnvSnapshot::capture(&text)?;
        self.db
            .set_setting(SETTING, &serde_json::to_string(&snapshot)?)?;
        Ok(snapshot)
    }

    fn config_credential(&self, target: &ConfigTarget) -> Result<Option<SecretString>> {
        if !target.disable_custom_auth
            || target.mode == ConnectionMode::Proxy
            || target.provider.official
        {
            return Ok(None);
        }
        let encrypted = self.db.secret(&target.provider.id)?;
        self.crypto
            .decrypt_for(&target.provider, &encrypted)
            .map(Some)
    }

    fn manages_codex_auth(target: &ConfigTarget) -> bool {
        target.client == ClientKind::Codex && !target.provider.official
    }

    fn managed_codex_auth_key<'a>(
        target: &ConfigTarget,
        credential: Option<&'a str>,
    ) -> Result<&'a str> {
        if target.disable_custom_auth && target.mode == ConnectionMode::Direct {
            credential.ok_or(DaemonError::Locked)
        } else {
            Ok(config::HSIN_MANAGED_KEY)
        }
    }

    fn codex_auth_backup(&self) -> Result<Option<config::CodexAuthSnapshot>> {
        let Some(encrypted) = self.db.protected_value(CODEX_AUTH_BACKUP_KEY)? else {
            return Ok(None);
        };
        let plaintext = self.crypto.decrypt_protected(&encrypted)?;
        serde_json::from_slice(&plaintext)
            .map(Some)
            .map_err(|_| DaemonError::Crypto)
    }

    fn ensure_codex_auth_backup(&self, path: &std::path::Path) -> Result<bool> {
        if let Some(snapshot) = self.codex_auth_backup()? {
            if std::path::Path::new(&snapshot.auth_path) != path {
                return Err(DaemonError::Conflict(
                    "Codex auth backup belongs to a different CODEX_HOME".into(),
                ));
            }
            return Ok(false);
        }
        let snapshot = config::capture_codex_auth(path)?;
        let serialized = Zeroizing::new(serde_json::to_vec(&snapshot)?);
        let encrypted = self
            .crypto
            .encrypt_protected(CODEX_AUTH_BACKUP_KEY, &serialized)?;
        self.db.put_protected_value(&encrypted)?;
        Ok(true)
    }

    fn remove_codex_auth_backup(&self) -> Result<()> {
        self.db.delete_protected_value(CODEX_AUTH_BACKUP_KEY)
    }

    fn apply_codex_auth_target(
        &self,
        target: &ConfigTarget,
        credential: Option<&str>,
    ) -> Result<()> {
        if target.client != ClientKind::Codex {
            return Ok(());
        }
        let config_path = self.config_path(ClientKind::Codex)?;
        let auth_path = config::codex_auth_path(&config_path)?;
        if Self::manages_codex_auth(target) {
            let key = Self::managed_codex_auth_key(target, credential)?;
            config::apply_codex_auth(&auth_path, target.codex_auth_before_hash.as_deref(), key)?;
        } else if let Some(snapshot) = self.codex_auth_backup()? {
            if std::path::Path::new(&snapshot.auth_path) != auth_path {
                return Err(DaemonError::Conflict(
                    "Codex auth backup belongs to a different CODEX_HOME".into(),
                ));
            }
            config::restore_codex_auth(
                &auth_path,
                target.codex_auth_before_hash.as_deref(),
                &snapshot,
            )?;
        }
        Ok(())
    }

    fn codex_auth_target_is_applied(
        &self,
        target: &ConfigTarget,
        credential: Option<&str>,
    ) -> Result<bool> {
        if target.client != ClientKind::Codex {
            return Ok(true);
        }
        let config_path = self.config_path(ClientKind::Codex)?;
        let auth_path = config::codex_auth_path(&config_path)?;
        let current = if auth_path.exists() {
            fs::read_to_string(&auth_path)?
        } else {
            String::new()
        };
        if Self::manages_codex_auth(target) {
            let key = Self::managed_codex_auth_key(target, credential)?;
            return config::codex_auth_is_managed(&current, key);
        }
        let Some(snapshot) = self.codex_auth_backup()? else {
            return Ok(true);
        };
        if std::path::Path::new(&snapshot.auth_path) != auth_path {
            return Err(DaemonError::Conflict(
                "Codex auth backup belongs to a different CODEX_HOME".into(),
            ));
        }
        Ok(config::restore_codex_auth_text(&current, &snapshot)? == current)
    }

    fn recover_codex_auth_target(
        &self,
        target: &ConfigTarget,
        credential: Option<&str>,
    ) -> Result<()> {
        if target.client != ClientKind::Codex {
            return Ok(());
        }
        let config_path = self.config_path(ClientKind::Codex)?;
        let auth_path = config::codex_auth_path(&config_path)?;
        if Self::manages_codex_auth(target) {
            self.ensure_codex_auth_backup(&auth_path)?;
        }
        if self.codex_auth_target_is_applied(target, credential)? {
            if !Self::manages_codex_auth(target) {
                self.remove_codex_auth_backup()?;
            }
            return Ok(());
        }
        let current_hash = config::file_hash(&auth_path)?;
        if current_hash != target.codex_auth_before_hash {
            return Err(DaemonError::Conflict(
                "Codex authentication changed during configuration recovery".into(),
            ));
        }
        self.apply_codex_auth_target(target, credential)?;
        if !Self::manages_codex_auth(target) {
            self.remove_codex_auth_backup()?;
        }
        Ok(())
    }

    pub fn recover_operations(&self) -> Result<()> {
        for (id, kind, client, before_hash, target_json) in self.db.pending_operations()? {
            let outcome = match kind.as_str() {
                "apply_config" | "edit_active_config" => {
                    self.recover_operation(client, before_hash.as_deref(), &target_json)
                }
                "import_official_auth" => self
                    .recover_official_import_operation(client, before_hash.as_deref(), &target_json)
                    .map(|()| RecoveryOutcome::Complete),
                _ => Err(DaemonError::Config(format!(
                    "unknown pending operation kind {kind}"
                ))),
            };
            match outcome {
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

    fn recover_official_import_operation(
        &self,
        client: ClientKind,
        before_hash: Option<&str>,
        target_json: &str,
    ) -> Result<()> {
        let mut target: ConfigTarget = serde_json::from_str(target_json)?;
        if client != ClientKind::Codex
            || target.client != client
            || target.mode != ConnectionMode::Direct
            || !target.provider.official
            || target.codex_auth_before_hash.as_deref() != before_hash
        {
            return Err(DaemonError::Conflict(
                "official import recovery target is invalid".into(),
            ));
        }
        let persisted = self.db.get_provider(&target.provider.id)?;
        target.provider.official = persisted.official;
        target.provider.credential_configured = persisted.credential_configured;
        // A journal row written before the model-mapping field existed deserializes it as `None`;
        // adopt the persisted value so the comparison below is not a false conflict.
        target
            .provider
            .claude_model_mapping
            .clone_from(&persisted.claude_model_mapping);
        if persisted != target.provider {
            return Err(DaemonError::Conflict(
                "official provider state diverged during import recovery".into(),
            ));
        }
        self.recover_codex_auth_target(&target, None)?;
        self.db
            .set_active(client, &target.provider.id, "synchronized")?;
        self.db.set_mode(client, ConnectionMode::Direct)
    }

    fn recover_operation(
        &self,
        client: ClientKind,
        before_hash: Option<&str>,
        target_json: &str,
    ) -> Result<RecoveryOutcome> {
        let mut target: ConfigTarget = serde_json::from_str(target_json)?;
        let persisted = self.db.get_provider(&target.provider.id)?;
        target.provider.official = persisted.official;
        target.provider.credential_configured = persisted.credential_configured;
        // A journal row written before the model-mapping field existed deserializes it as `None`;
        // adopt the persisted value so the comparison below is not a false conflict.
        target
            .provider
            .claude_model_mapping
            .clone_from(&persisted.claude_model_mapping);
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
        let credential = self.config_credential(&target)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        if current_hash.as_deref() == before_hash {
            config::apply_with_credential(&path, before_hash, &target, credential)?;
        } else if config::patch_text_with_credential(&current, &target, credential)? != current {
            return Err(DaemonError::Conflict(
                "configuration diverged during recovery".into(),
            ));
        }
        self.recover_codex_auth_target(&target, credential)?;
        self.db
            .set_active(client, &target.provider.id, "synchronized")?;
        self.db.set_mode(client, target.mode)?;
        let mut client_auth = self.client_auth_settings()?;
        client_auth.set_disable_custom_auth(client, target.disable_custom_auth);
        self.db
            .set_setting("client_auth", &serde_json::to_string(&client_auth)?)?;
        Ok(RecoveryOutcome::Complete)
    }

    pub fn status(&self) -> Result<DaemonStatus> {
        Ok(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            locked: !self.crypto.is_unlocked(),
            proxy_listening: self.proxy_listening.load(Ordering::Acquire),
            proxy_enabled: self.proxy_enabled(),
            proxy_address: self.proxy_address()?.to_string(),
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
                .unwrap_or_else(|| hsin_core::LANGUAGE_SYSTEM.into()),
            proxy_host: self.proxy_host()?.to_string(),
            proxy_port: self.proxy_port()?,
            proxy_enabled: self.proxy_enabled(),
            clients: self.client_settings()?,
            client_auth: self.client_auth_settings()?,
        })
    }

    fn client_settings(&self) -> Result<ClientSettings> {
        let Some(value) = self.db.setting("clients")? else {
            return Ok(ClientSettings::default());
        };
        let settings: ClientSettings = serde_json::from_str(&value)
            .map_err(|error| DaemonError::Config(format!("invalid client settings: {error}")))?;
        if !settings.is_valid() {
            return Err(DaemonError::Config("invalid client settings".into()));
        }
        Ok(settings)
    }

    fn client_auth_settings(&self) -> Result<ClientAuthSettings> {
        let Some(value) = self.db.setting("client_auth")? else {
            return Ok(ClientAuthSettings::default());
        };
        serde_json::from_str(&value)
            .map_err(|error| DaemonError::Config(format!("invalid client auth settings: {error}")))
    }

    pub fn disable_custom_auth(&self, client: ClientKind) -> Result<bool> {
        Ok(self.client_auth_settings()?.disable_custom_auth(client))
    }

    pub async fn update_settings(&self, patch: SettingsPatch) -> Result<Settings> {
        let _guard = self.mutation.lock().await;
        let SettingsPatch {
            language,
            proxy_host,
            proxy_port,
            proxy_enabled,
            clients,
            client_auth,
        } = patch;
        if clients.as_ref().is_some_and(|clients| !clients.is_valid()) {
            return Err(DaemonError::Invalid(
                "client settings must contain each client once and keep at least one visible"
                    .into(),
            ));
        }
        let old_host = self.proxy_host()?;
        let old_port = self.proxy_port()?;
        let new_host = match proxy_host {
            Some(host) => host
                .trim()
                .parse::<IpAddr>()
                .map_err(|_| DaemonError::Invalid("proxy host must be an IP address".into()))?,
            None => old_host,
        };
        let new_port = proxy_port.unwrap_or(old_port);
        if new_port < 1024 {
            return Err(DaemonError::Invalid(
                "proxy port must be at least 1024".into(),
            ));
        }
        if let Some(language) = language {
            if !matches!(
                language.as_str(),
                hsin_core::LANGUAGE_SYSTEM | hsin_core::LANGUAGE_EN_US | hsin_core::LANGUAGE_ZH_CN
            ) {
                return Err(DaemonError::Invalid(
                    "language must be system, en-US, or zh-CN".into(),
                ));
            }
            self.db.set_setting("language", &language)?;
        }
        if proxy_enabled == Some(false) {
            self.disable_all_client_proxies_locked()?;
            self.set_proxy_enabled_locked(false).await?;
        }
        let binding_changed = new_host != old_host || new_port != old_port;
        if binding_changed {
            self.set_proxy_binding_settings(new_host, new_port)?;
            if let Err(error) = self.reconcile_proxy_configurations() {
                self.set_proxy_binding_settings(old_host, old_port)?;
                let _ = self.reconcile_proxy_configurations();
                return Err(error);
            }
            if self.proxy_enabled()
                && let Err(error) = self.restart_proxy_locked().await
            {
                self.set_proxy_binding_settings(old_host, old_port)?;
                let _ = self.reconcile_proxy_configurations();
                let _ = self.restart_proxy_locked().await;
                return Err(error);
            }
        }
        if let Some(clients) = clients {
            self.db
                .set_setting("clients", &serde_json::to_string(&clients)?)?;
        }
        if let Some(update) = client_auth {
            let mut client_auth = self.client_auth_settings()?;
            if update.disable_custom_auth != client_auth.disable_custom_auth(update.client) {
                let state = self.db.client_state(update.client)?;
                if let Some(provider_id) = state.active_provider_id {
                    let provider = self.db.get_provider(&provider_id)?;
                    self.apply_configuration_with_auth(
                        &provider,
                        state.mode,
                        Some(update.disable_custom_auth),
                    )?;
                }
                client_auth.set_disable_custom_auth(update.client, update.disable_custom_auth);
                self.db
                    .set_setting("client_auth", &serde_json::to_string(&client_auth)?)?;
            }
        }
        if proxy_enabled == Some(true) {
            self.set_proxy_enabled_locked(true).await?;
        }
        self.settings()
    }

    fn set_proxy_binding_settings(&self, host: IpAddr, port: u16) -> Result<()> {
        self.db.set_setting("proxy_host", &host.to_string())?;
        self.db.set_setting("proxy_port", &port.to_string())
    }

    pub(crate) fn reconcile_proxy_configurations(&self) -> Result<()> {
        for client in ClientKind::ALL {
            let state = self.db.client_state(client)?;
            if state.mode != ConnectionMode::Proxy {
                continue;
            }
            if let Some(provider_id) = state.active_provider_id {
                let provider = self.db.get_provider(&provider_id)?;
                self.apply_configuration(&provider, ConnectionMode::Proxy)?;
            }
        }
        Ok(())
    }

    fn disable_all_client_proxies_locked(&self) -> Result<()> {
        for client in [ClientKind::Codex, ClientKind::Claude] {
            let state = self.db.client_state(client)?;
            if state.mode == ConnectionMode::Direct {
                continue;
            }
            if let Some(provider_id) = state.active_provider_id {
                let provider = self.db.get_provider(&provider_id)?;
                self.apply_configuration(&provider, ConnectionMode::Direct)?;
            } else {
                self.db.set_mode(client, ConnectionMode::Direct)?;
            }
        }
        Ok(())
    }

    async fn set_proxy_enabled_locked(&self, enabled: bool) -> Result<()> {
        if self.proxy_enabled() == enabled
            && self.proxy_listening.load(Ordering::Acquire) == enabled
        {
            return Ok(());
        }
        if self.proxy_enabled() != enabled {
            self.db
                .set_setting("proxy_enabled", if enabled { "true" } else { "false" })?;
        }
        let address = self.proxy_address()?;
        self.replace_proxy_runtime(enabled, address);
        if self.wait_for_proxy_state(enabled, address).await {
            return Ok(());
        }
        if enabled {
            self.db.set_setting("proxy_enabled", "false")?;
            self.replace_proxy_runtime(false, address);
            let reason = self.proxy_failure.read().clone().map_or_else(
                || "proxy did not become ready".to_string(),
                |reason| format!("proxy could not listen on {address}: {reason}"),
            );
            return Err(DaemonError::Config(reason));
        }
        Err(DaemonError::Config("proxy did not stop in time".into()))
    }

    async fn restart_proxy_locked(&self) -> Result<()> {
        let address = self.proxy_address()?;
        self.replace_proxy_runtime(true, address);
        if self.wait_for_proxy_state(true, address).await {
            Ok(())
        } else {
            Err(DaemonError::Config(
                "proxy did not restart with the updated address".into(),
            ))
        }
    }

    fn replace_proxy_runtime(&self, enabled: bool, address: SocketAddr) {
        let revision = self.proxy_runtime.borrow().revision.wrapping_add(1);
        self.proxy_runtime.send_replace(ProxyRuntimeConfig {
            enabled,
            address,
            revision,
        });
    }

    async fn wait_for_proxy_state(&self, enabled: bool, address: SocketAddr) -> bool {
        for _ in 0..100 {
            let ready = if enabled {
                *self.proxy_active_address.read() == Some(address)
            } else {
                !self.proxy_listening.load(Ordering::Acquire)
            };
            if ready {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
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
            recovery_key_configured: self.recovery_key_exported()?,
        })
    }

    fn recovery_key_exported(&self) -> Result<bool> {
        Ok(self
            .db
            .setting(RECOVERY_KEY_EXPORTED_KEY)?
            .is_some_and(|value| !value.is_empty()))
    }

    pub fn export_recovery_key(&self) -> Result<String> {
        let recovery = self
            .crypto
            .export_recovery_key()?
            .expose_secret()
            .to_owned();
        self.mark_recovery_key_held()?;
        Ok(recovery)
    }
    pub fn import_recovery_key(&self, value: &str) -> Result<()> {
        self.crypto.import_recovery_key(value)?;
        // A successful import proves the operator still holds the key.
        self.mark_recovery_key_held()
    }
    pub async fn rotate_key(&self) -> Result<u32> {
        let _guard = self.mutation.lock().await;
        let version = self.crypto.rotate()?;
        // Rotation invalidates every previously exported recovery key, because
        // an import only accepts the current key version.
        self.db.set_setting(RECOVERY_KEY_EXPORTED_KEY, "")?;
        Ok(version)
    }

    fn mark_recovery_key_held(&self) -> Result<()> {
        self.db.set_setting(RECOVERY_KEY_EXPORTED_KEY, "1")
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
        // Without an exported recovery key, losing the keyring entry destroys
        // every stored credential, so warn for as long as none is held.
        if self.db.current_key_record()?.is_some() && !self.recovery_key_exported()? {
            findings.push(finding(
                "recovery_key_not_exported",
                DoctorSeverity::Warning,
            ));
        }
        if self.db.integrity_check()? != "ok" {
            findings.push(finding("database_integrity_failed", DoctorSeverity::Error));
        }
        if self.proxy_enabled() && !self.proxy_listening.load(Ordering::Acquire) {
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

fn base_url_has_v1(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.path_segments()
            .and_then(|segments| segments.rev().find(|segment| !segment.is_empty()))
            .is_some_and(|segment| segment.eq_ignore_ascii_case("v1"))
    })
}

fn parse_model_ids(value: &serde_json::Value) -> Result<Vec<String>> {
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DaemonError::Config("model response is missing a model array".into()))?;
    let mut models = entries
        .iter()
        .filter_map(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("id").and_then(serde_json::Value::as_str))
                .or_else(|| entry.get("name").and_then(serde_json::Value::as_str))
        })
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .to_ascii_lowercase()
            .cmp(&left.to_ascii_lowercase())
            .then_with(|| right.cmp(left))
    });
    models.dedup();
    Ok(models)
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
            DaemonError::OAuthProxyUnsupported => ErrorCode::OAuthProxyUnsupported,
            DaemonError::NoActiveProvider => ErrorCode::NoActiveProvider,
            DaemonError::CurrentCredentialUnavailable => ErrorCode::CurrentCredentialUnavailable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, fs};

    use axum::{Json, Router, http::StatusCode, routing::get};
    use parking_lot::Mutex as ParkingMutex;
    use serde_json::json;

    use crate::{crypto::KeyStore, paths::Paths};

    #[derive(Default)]
    struct MemoryStore(ParkingMutex<HashMap<u32, String>>);

    impl KeyStore for MemoryStore {
        fn load(&self, version: u32) -> Result<Option<String>> {
            Ok(self.0.lock().get(&version).cloned())
        }

        fn store(&self, version: u32, value: &str) -> Result<()> {
            self.0.lock().insert(version, value.to_owned());
            Ok(())
        }

        fn delete(&self, version: u32) -> Result<()> {
            self.0.lock().remove(&version);
            Ok(())
        }
    }

    #[test]
    fn wildcard_listener_hosts_use_loopback_client_destinations() {
        assert_eq!(
            client_host_for_listener("0.0.0.0".parse().unwrap()),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_host_for_listener("::".parse().unwrap()),
            "::1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_host_for_listener("192.168.1.10".parse().unwrap()),
            "192.168.1.10".parse::<IpAddr>().unwrap()
        );
    }

    fn leave_pending_configuration_operation(
        app: &App,
        path: &std::path::Path,
        target: &ConfigTarget,
    ) {
        let before_hash = config::file_hash(path).unwrap();
        app.db
            .begin_operation(
                "apply_config",
                target.client,
                before_hash.as_deref(),
                &serde_json::to_string(target).unwrap(),
            )
            .unwrap();
        let credential = app.config_credential(target).unwrap();
        config::apply_with_credential(
            path,
            before_hash.as_deref(),
            target,
            credential.as_ref().map(ExposeSecret::expose_secret),
        )
        .unwrap();
    }

    #[test]
    fn model_ids_are_sorted_descending() {
        let models = parse_model_ids(&json!({
            "data": [{"id":"gpt-4.1"}, {"id":"gpt-5"}, {"id":"alpha"}]
        }))
        .unwrap();
        assert_eq!(models, ["gpt-5", "gpt-4.1", "alpha"]);
    }

    #[test]
    fn credential_previews_reveal_only_a_small_prefix_and_suffix() {
        assert_eq!(
            credential_preview("sk-abcdefghijklmnopqrstuvwxyz"),
            "sk-abc***yz"
        );
        assert_eq!(credential_preview("short-key"), "••••••••");
    }

    #[tokio::test]
    async fn a_recovered_proxy_clears_the_recorded_failure() {
        // A stale "address already in use" would be attached to the next
        // unrelated timeout and send operators after the wrong port.
        let root = std::env::temp_dir().join(format!("hsind-proxyfail-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();

        app.mark_proxy_failure("I/O error: Address already in use (os error 98)");
        assert!(app.proxy_failure.read().is_some());

        app.mark_proxy_active(Some(([127, 0, 0, 1], 9999).into()));
        assert!(
            app.proxy_failure.read().is_none(),
            "a listener that came up invalidates the previous failure"
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn recovery_key_status_tracks_the_actual_export() {
        // Reporting a recovery key that was never exported would tell operators
        // they are protected while a lost keyring entry destroys every secret.
        let root = std::env::temp_dir().join(format!("hsind-recovery-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();

        let status = app.security_status().unwrap();
        assert_eq!(status.key_version, 1, "a master key exists");
        assert!(
            !status.recovery_key_configured,
            "nothing was exported yet, so no recovery key is held"
        );
        assert!(
            app.doctor()
                .unwrap()
                .findings
                .iter()
                .any(|item| item.code == "recovery_key_not_exported"),
            "doctor must surface the missing recovery key"
        );

        let recovery = app.export_recovery_key().unwrap();
        assert!(app.security_status().unwrap().recovery_key_configured);

        // Rotation invalidates the exported key, so the warning must return.
        app.rotate_key().await.unwrap();
        assert!(
            !app.security_status().unwrap().recovery_key_configured,
            "the exported key no longer matches the current key version"
        );
        assert!(app.import_recovery_key(&recovery).is_err());

        // Re-exporting after rotation restores the held state.
        app.export_recovery_key().unwrap();
        assert!(app.security_status().unwrap().recovery_key_configured);

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_list_never_returns_a_complete_credential() {
        let root = std::env::temp_dir().join(format!("hsind-preview-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        let secret = "sk-abcdefghijklmnopqrstuvwxyz";
        app.add_provider(ProviderAddParams {
            provider: hsin_core::ProviderDraft {
                client: ClientKind::Codex,
                name: "Preview".into(),
                description: String::new(),
                base_url: "https://preview.example.test/v1".into(),
                auth_scheme: AuthScheme::Bearer,
                model: None,
                claude_model_mapping: None,
            },
            secret: SecretInput::Replace(secret.into()),
        })
        .await
        .unwrap();

        let providers = app
            .list_providers(&ProviderListParams {
                client: Some(ClientKind::Codex),
            })
            .unwrap();
        let preview = providers[0].credential_preview.as_deref().unwrap();
        assert_eq!(preview, "sk-abc***yz");
        assert!(!serde_json::to_string(&providers).unwrap().contains(secret));
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn client_settings_are_validated_and_persisted() {
        let root = std::env::temp_dir().join(format!("hsind-clients-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        let clients = ClientSettings {
            order: vec![ClientKind::Claude, ClientKind::Codex],
            visible: vec![ClientKind::Claude],
        };
        let settings = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(clients.clone()),
                client_auth: None,
            })
            .await
            .unwrap();
        assert_eq!(settings.clients, clients);
        assert_eq!(app.settings().unwrap().clients, clients);

        let invalid = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(ClientSettings {
                    order: ClientKind::ALL.to_vec(),
                    visible: Vec::new(),
                }),
                client_auth: None,
            })
            .await;
        assert!(matches!(invalid, Err(DaemonError::Invalid(_))));
        assert_eq!(app.settings().unwrap().clients, clients);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn client_auth_setting_rewrites_the_active_direct_configuration() {
        let root = std::env::temp_dir().join(format!("hsind-client-auth-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        let codex_auth = root.join("codex/auth.json");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        let original_auth = "{\n  \"auth_mode\": \"chatgpt\",\n  \"tokens\": {\"access_token\": \"keep-token\"},\n  \"account_id\": \"keep-account\"\n}\n";
        fs::write(&codex_auth, original_auth).unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let provider = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Codex custom".into(),
                    description: String::new(),
                    base_url: "https://codex.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("sk-client-auth-secret".into()),
            })
            .await
            .unwrap();
        app.switch_provider(ProviderSwitchParams {
            client: ClientKind::Codex,
            provider_id: provider.id,
        })
        .await
        .unwrap();
        assert!(
            fs::read_to_string(&codex_config)
                .unwrap()
                .contains("[model_providers.hsin.auth]")
        );
        let auth = fs::read_to_string(&codex_auth).unwrap();
        assert!(auth.contains("\"auth_mode\": \"apikey\""));
        assert!(auth.contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));
        assert!(auth.contains("\"tokens\": {\"access_token\": \"keep-token\"}"));
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_some()
        );

        let client_auth = ClientAuthSettings {
            codex_disable_custom_auth: true,
            claude_disable_custom_auth: false,
        };
        let settings = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: Some(hsin_core::ClientAuthUpdate {
                    client: ClientKind::Codex,
                    disable_custom_auth: true,
                }),
            })
            .await
            .unwrap();
        assert_eq!(settings.client_auth, client_auth);
        let configured = fs::read_to_string(&codex_config).unwrap();
        assert!(configured.contains("requires_openai_auth = true"));
        assert!(!configured.contains("sk-client-auth-secret"));
        assert!(!configured.contains("[model_providers.hsin.auth]"));
        let auth = fs::read_to_string(&codex_auth).unwrap();
        assert!(auth.contains("\"auth_mode\": \"apikey\""));
        assert!(auth.contains("\"OPENAI_API_KEY\": \"sk-client-auth-secret\""));
        assert!(auth.contains("\"tokens\": {\"access_token\": \"keep-token\"}"));
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_some()
        );
        let audit = rusqlite::Connection::open(&paths.database).unwrap();
        let leaked: i64 = audit
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE instr(target_json,'sk-client-auth-secret')>0) OR EXISTS(SELECT 1 FROM settings WHERE instr(value,'sk-client-auth-secret')>0)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leaked, 0);
        drop(audit);

        let settings = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: None,
                client_auth: Some(hsin_core::ClientAuthUpdate {
                    client: ClientKind::Codex,
                    disable_custom_auth: false,
                }),
            })
            .await
            .unwrap();
        assert!(!settings.client_auth.codex_disable_custom_auth);
        let auth = fs::read_to_string(&codex_auth).unwrap();
        assert!(auth.contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));
        assert!(auth.contains("\"tokens\": {\"access_token\": \"keep-token\"}"));
        assert!(
            fs::read_to_string(&codex_config)
                .unwrap()
                .contains("[model_providers.hsin.auth]")
        );
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_some()
        );

        app.update_settings(SettingsPatch {
            language: None,
            proxy_host: None,
            proxy_port: None,
            proxy_enabled: None,
            clients: None,
            client_auth: Some(hsin_core::ClientAuthUpdate {
                client: ClientKind::Codex,
                disable_custom_auth: true,
            }),
        })
        .await
        .unwrap();
        let official = app.ensure_official_provider(ClientKind::Codex).unwrap();
        app.switch_provider(ProviderSwitchParams {
            client: ClientKind::Codex,
            provider_id: official.id,
        })
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&codex_auth).unwrap(), original_auth);
        assert!(
            app.settings()
                .unwrap()
                .client_auth
                .codex_disable_custom_auth
        );
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_none()
        );

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn configuration_recovery_completes_codex_auth_write_and_restore() {
        let root =
            std::env::temp_dir().join(format!("hsind-auth-recovery-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        let codex_auth = root.join("codex/auth.json");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        let original_auth =
            "{\n  \"auth_mode\": \"chatgpt\",\n  \"tokens\": {\"access_token\": \"keep\"}\n}\n";
        fs::write(&codex_auth, original_auth).unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let provider = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Recovery".into(),
                    description: String::new(),
                    base_url: "https://recovery.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("recovery-secret".into()),
            })
            .await
            .unwrap();

        let enable = app
            .config_target(&provider, ConnectionMode::Direct, Some(true))
            .unwrap();
        app.ensure_codex_auth_backup(&codex_auth).unwrap();
        leave_pending_configuration_operation(&app, &codex_config, &enable);
        app.recover_operations().unwrap();
        assert!(
            fs::read_to_string(&codex_auth)
                .unwrap()
                .contains("\"OPENAI_API_KEY\": \"recovery-secret\"")
        );
        assert!(
            app.settings()
                .unwrap()
                .client_auth
                .codex_disable_custom_auth
        );
        assert!(app.db.pending_operations().unwrap().is_empty());

        let helper_auth = app
            .config_target(&provider, ConnectionMode::Direct, Some(false))
            .unwrap();
        leave_pending_configuration_operation(&app, &codex_config, &helper_auth);
        app.recover_operations().unwrap();
        assert!(fs::read_to_string(&codex_auth).unwrap().contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));
        assert!(
            !app.settings()
                .unwrap()
                .client_auth
                .codex_disable_custom_auth
        );
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_some()
        );
        assert!(app.db.pending_operations().unwrap().is_empty());

        let official = app.ensure_official_provider(ClientKind::Codex).unwrap();
        let restore = app
            .config_target(&official, ConnectionMode::Direct, Some(false))
            .unwrap();
        leave_pending_configuration_operation(&app, &codex_config, &restore);
        app.recover_operations().unwrap();
        assert_eq!(fs::read_to_string(&codex_auth).unwrap(), original_auth);
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_none()
        );
        assert!(app.db.pending_operations().unwrap().is_empty());

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn startup_reconciles_the_previous_experimental_bearer_token_format() {
        let root =
            std::env::temp_dir().join(format!("hsind-auth-upgrade-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        fs::write(
            &codex_config,
            format!(
                "model_provider = \"hsin\"\n[model_providers.hsin]\nname = \"hsin\"\nbase_url = \"http://127.0.0.1:9999/codex/v1\"\nwire_api = \"responses\"\nexperimental_bearer_token = \"{}\"\n",
                config::HSIN_MANAGED_KEY
            ),
        )
        .unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let provider = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Upgrade".into(),
                    description: String::new(),
                    base_url: "https://upgrade.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("upgrade-secret".into()),
            })
            .await
            .unwrap();
        app.db
            .set_active(ClientKind::Codex, &provider.id, "synchronized")
            .unwrap();
        app.db
            .set_mode(ClientKind::Codex, ConnectionMode::Proxy)
            .unwrap();
        app.db
            .set_setting(
                "client_auth",
                &serde_json::to_string(&ClientAuthSettings {
                    codex_disable_custom_auth: true,
                    claude_disable_custom_auth: false,
                })
                .unwrap(),
            )
            .unwrap();

        app.reconcile_client_auth_configuration().unwrap();
        let configured = fs::read_to_string(&codex_config).unwrap();
        assert!(configured.contains("requires_openai_auth = true"));
        assert!(!configured.contains("experimental_bearer_token"));
        let auth = fs::read_to_string(root.join("codex/auth.json")).unwrap();
        assert!(auth.contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn startup_reconciles_cli_auth_with_codex_login_placeholder() {
        let root = std::env::temp_dir().join(format!("hsind-auth-helper-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        let codex_auth = root.join("codex/auth.json");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        let original_auth =
            "{\n  \"auth_mode\": \"chatgpt\",\n  \"tokens\": {\"access_token\": \"keep\"}\n}\n";
        fs::write(&codex_auth, original_auth).unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let provider = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Helper login".into(),
                    description: String::new(),
                    base_url: "https://helper.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("helper-secret".into()),
            })
            .await
            .unwrap();
        app.db
            .set_active(ClientKind::Codex, &provider.id, "synchronized")
            .unwrap();
        app.db
            .set_mode(ClientKind::Codex, ConnectionMode::Proxy)
            .unwrap();

        app.reconcile_client_auth_configuration().unwrap();

        let configured = fs::read_to_string(&codex_config).unwrap();
        assert!(configured.contains("[model_providers.hsin.auth]"));
        assert!(configured.contains("args = [\"credential\", \"codex\"]"));
        assert!(!configured.contains("requires_openai_auth"));
        assert!(!configured.contains("helper-secret"));
        let auth = fs::read_to_string(&codex_auth).unwrap();
        assert!(auth.contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));
        assert!(auth.contains("\"tokens\": {\"access_token\": \"keep\"}"));
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_some()
        );

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v1_detection_checks_the_last_path_segment() {
        assert!(base_url_has_v1("https://example.test/v1"));
        assert!(base_url_has_v1("https://example.test/api/V1/"));
        assert!(!base_url_has_v1("https://example.test/api"));
    }

    #[tokio::test]
    async fn proxy_mode_without_an_active_provider_has_a_specific_error() {
        let root = std::env::temp_dir().join(format!("hsind-empty-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        assert!(matches!(
            app.set_mode(ClientKind::Codex, ConnectionMode::Proxy).await,
            Err(DaemonError::NoActiveProvider)
        ));
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_start_creates_official_and_imports_the_active_third_party_provider() {
        let root = std::env::temp_dir().join(format!("hsind-import-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex = root.join("codex/config.toml");
        let claude = root.join("claude/settings.json");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::write(&codex, "model_provider = \"openai\"\n").unwrap();
        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.test","ANTHROPIC_API_KEY":"secret"}}"#,
        )
        .unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() =
            HashMap::from([(ClientKind::Codex, codex), (ClientKind::Claude, claude)]);

        app.initialize_providers().unwrap();
        app.initialize_providers().unwrap();

        let codex_providers = app.db.list_providers(Some(ClientKind::Codex)).unwrap();
        assert_eq!(codex_providers.len(), 1);
        assert!(codex_providers[0].official);
        assert_eq!(codex_providers[0].auth_scheme, AuthScheme::OAuth);
        assert_eq!(
            app.db
                .client_state(ClientKind::Codex)
                .unwrap()
                .active_provider_id
                .as_deref(),
            Some("official-codex")
        );

        let claude_providers = app.db.list_providers(Some(ClientKind::Claude)).unwrap();
        assert_eq!(claude_providers.len(), 2);
        let imported = claude_providers
            .iter()
            .find(|provider| !provider.official)
            .unwrap();
        assert!(imported.credential_configured);
        assert!(app.db.secret(&imported.id).is_ok());
        assert_eq!(
            app.db
                .client_state(ClientKind::Claude)
                .unwrap()
                .active_provider_id
                .as_deref(),
            Some(imported.id.as_str())
        );
        assert_eq!(
            app.db
                .setting("providers_initialized_v1")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn importing_official_codex_restores_auth_without_rewriting_config() {
        let root =
            std::env::temp_dir().join(format!("hsind-import-oauth-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        let codex_auth = root.join("codex/auth.json");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        let original_auth =
            "{\n  \"auth_mode\": \"chatgpt\",\n  \"tokens\": {\"access_token\": \"keep\"}\n}\n";
        fs::write(&codex_auth, original_auth).unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let custom = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Import OAuth".into(),
                    description: String::new(),
                    base_url: "https://oauth-import.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("import-secret".into()),
            })
            .await
            .unwrap();
        app.switch_provider(ProviderSwitchParams {
            client: ClientKind::Codex,
            provider_id: custom.id,
        })
        .await
        .unwrap();
        let official_config = "# keep\nmodel_provider = \"openai\"\napproval_policy = \"never\"\n";
        fs::write(&codex_config, official_config).unwrap();

        let result = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Codex,
                name: String::new(),
            })
            .await
            .unwrap();

        assert!(result.provider.official);
        assert!(!result.imported);
        assert_eq!(fs::read_to_string(&codex_config).unwrap(), official_config);
        assert_eq!(fs::read_to_string(&codex_auth).unwrap(), original_auth);
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_none()
        );
        let state = app.db.client_state(ClientKind::Codex).unwrap();
        assert_eq!(state.active_provider_id.as_deref(), Some("official-codex"));
        assert_eq!(state.mode, ConnectionMode::Direct);
        assert!(app.db.pending_operations().unwrap().is_empty());

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_completes_pending_official_codex_import() {
        let root =
            std::env::temp_dir().join(format!("hsind-recover-oauth-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex_config = root.join("codex/config.toml");
        let codex_auth = root.join("codex/auth.json");
        fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        let original_auth = "{\n  \"auth_mode\": \"chatgpt\"\n}\n";
        fs::write(&codex_auth, original_auth).unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex_config.clone()),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let custom = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Recover OAuth".into(),
                    description: String::new(),
                    base_url: "https://oauth-recovery.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("recovery-secret".into()),
            })
            .await
            .unwrap();
        app.switch_provider(ProviderSwitchParams {
            client: ClientKind::Codex,
            provider_id: custom.id,
        })
        .await
        .unwrap();
        let official_config = "model_provider = \"openai\"\n";
        fs::write(&codex_config, official_config).unwrap();
        let official = app.ensure_official_provider(ClientKind::Codex).unwrap();
        let target = app
            .config_target(&official, ConnectionMode::Direct, None)
            .unwrap();
        app.db
            .begin_operation(
                "import_official_auth",
                ClientKind::Codex,
                target.codex_auth_before_hash.as_deref(),
                &serde_json::to_string(&target).unwrap(),
            )
            .unwrap();

        app.recover_operations().unwrap();

        assert_eq!(fs::read_to_string(&codex_config).unwrap(), official_config);
        assert_eq!(fs::read_to_string(&codex_auth).unwrap(), original_auth);
        assert!(
            app.db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            app.db
                .client_state(ClientKind::Codex)
                .unwrap()
                .active_provider_id
                .as_deref(),
            Some("official-codex")
        );
        assert!(app.db.pending_operations().unwrap().is_empty());

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn import_current_deduplicates_by_url_and_secret() {
        let root = std::env::temp_dir().join(format!("hsind-reimport-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let codex = root.join("codex/config.toml");
        let claude = root.join("claude/settings.json");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::write(&codex, "model_provider = \"openai\"\n").unwrap();
        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.test","ANTHROPIC_API_KEY":"key-a"}}"#,
        )
        .unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, codex),
            (ClientKind::Claude, claude.clone()),
        ]);
        app.initialize_providers().unwrap();

        let unchanged = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Claude,
                name: String::new(),
            })
            .await
            .unwrap();
        assert!(!unchanged.imported);
        assert_eq!(
            app.db
                .list_providers(Some(ClientKind::Claude))
                .unwrap()
                .len(),
            2
        );

        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.test","ANTHROPIC_API_KEY":"key-b"}}"#,
        )
        .unwrap();
        let changed_key = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Claude,
                name: String::new(),
            })
            .await
            .unwrap();
        assert!(changed_key.imported);
        assert_eq!(
            app.db
                .list_providers(Some(ClientKind::Claude))
                .unwrap()
                .len(),
            3
        );

        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example.test","ANTHROPIC_API_KEY":"key-b"}}"#,
        )
        .unwrap();
        let changed_url = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Claude,
                name: String::new(),
            })
            .await
            .unwrap();
        assert!(changed_url.imported);
        assert_eq!(
            app.db
                .list_providers(Some(ClientKind::Claude))
                .unwrap()
                .len(),
            4
        );

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn importing_claude_official_url_respects_explicit_auth() {
        let root =
            std::env::temp_dir().join(format!("hsind-import-claude-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let claude = root.join("claude/settings.json");
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com","ANTHROPIC_API_KEY":"api-secret"}}"#,
        )
        .unwrap();
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, root.join("codex/config.toml")),
            (ClientKind::Claude, claude.clone()),
        ]);

        let imported = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Claude,
                name: "Official API key".into(),
            })
            .await
            .unwrap();

        assert!(imported.imported);
        assert!(!imported.provider.official);
        assert_eq!(imported.provider.auth_scheme, AuthScheme::XApiKey);
        assert!(imported.provider.credential_configured);
        assert!(app.db.secret(&imported.provider.id).is_ok());

        let helper_marker = root.join("helper-ran");
        fs::write(
            &claude,
            serde_json::to_vec(&json!({
                "env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com"},
                "apiKeyHelper": format!("touch {}", helper_marker.display()),
            }))
            .unwrap(),
        )
        .unwrap();
        let helper_import = app
            .import_current(ImportCurrentParams {
                client: ClientKind::Claude,
                name: String::new(),
            })
            .await;
        assert!(matches!(
            helper_import,
            Err(DaemonError::CurrentCredentialUnavailable)
        ));
        assert!(!helper_marker.exists());
        assert_eq!(
            app.db
                .client_state(ClientKind::Claude)
                .unwrap()
                .active_provider_id,
            Some(imported.provider.id)
        );

        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn model_discovery_retries_with_v1_and_returns_resolved_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route(
                "/models",
                get(|| async { (StatusCode::NOT_FOUND, "missing") }),
            )
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data": [{"id": "gpt-4.1"}, {"id": "gpt-5"}]})) }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let root = std::env::temp_dir().join(format!("hsind-models-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        let discovery = app
            .discover_models(ModelDiscoverParams {
                client: ClientKind::Codex,
                provider_id: None,
                base_url: format!("http://{address}"),
                auth_scheme: AuthScheme::Bearer,
                secret: SecretInput::Replace("test-key".into()),
            })
            .await
            .unwrap();
        assert_eq!(discovery.resolved_base_url, format!("http://{address}/v1"));
        assert_eq!(discovery.models, ["gpt-5", "gpt-4.1"]);

        server.abort();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn client_proxy_toggle_enables_global_proxy_and_global_off_disables_all_clients() {
        let root = std::env::temp_dir().join(format!("hsind-toggle-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: root.join("hsin.sqlite3"),
            lock: root.join("hsind.lock"),
            logs: root.join("logs"),
            backups: root.join("backups"),
            home: root.clone(),
        };
        let app = App::open_with_store(&paths, Arc::new(MemoryStore::default())).unwrap();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, root.join("codex/config.toml")),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let disabled_settings = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: Some("127.0.0.1".into()),
                proxy_port: Some(port),
                proxy_enabled: None,
                clients: None,
                client_auth: None,
            })
            .await
            .unwrap();
        assert_eq!(disabled_settings.proxy_port, port);
        assert!(!disabled_settings.proxy_enabled);

        let codex = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Codex custom".into(),
                    description: String::new(),
                    base_url: "https://codex.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("codex-secret".into()),
            })
            .await
            .unwrap();
        let claude = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Claude,
                    name: "Claude custom".into(),
                    description: String::new(),
                    base_url: "https://claude.example.test".into(),
                    auth_scheme: AuthScheme::XApiKey,
                    model: None,
                    claude_model_mapping: None,
                },
                secret: SecretInput::Replace("claude-secret".into()),
            })
            .await
            .unwrap();
        app.db
            .set_active(ClientKind::Codex, &codex.id, "synchronized")
            .unwrap();
        app.db
            .set_active(ClientKind::Claude, &claude.id, "synchronized")
            .unwrap();
        app.update_settings(SettingsPatch {
            language: None,
            proxy_host: None,
            proxy_port: None,
            proxy_enabled: None,
            clients: None,
            client_auth: Some(hsin_core::ClientAuthUpdate {
                client: ClientKind::Codex,
                disable_custom_auth: true,
            }),
        })
        .await
        .unwrap();

        let proxy = tokio::spawn(crate::proxy::serve(app.clone()));
        tokio::task::yield_now().await;
        app.set_mode(ClientKind::Codex, ConnectionMode::Proxy)
            .await
            .unwrap();
        assert!(app.proxy_enabled());
        assert!(app.proxy_listening.load(Ordering::Acquire));
        assert_eq!(
            app.db.client_state(ClientKind::Codex).unwrap().mode,
            ConnectionMode::Proxy
        );
        let codex_auth = fs::read_to_string(root.join("codex/auth.json")).unwrap();
        assert!(codex_auth.contains(&format!(
            "\"OPENAI_API_KEY\": \"{}\"",
            config::HSIN_MANAGED_KEY
        )));

        let next_probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let next_port = next_probe.local_addr().unwrap().port();
        drop(next_probe);
        let restarted = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_host: Some("0.0.0.0".into()),
                proxy_port: Some(next_port),
                proxy_enabled: None,
                clients: None,
                client_auth: None,
            })
            .await
            .unwrap();
        assert_eq!(restarted.proxy_host, "0.0.0.0");
        assert_eq!(restarted.proxy_port, next_port);
        assert!(restarted.proxy_enabled);
        assert_eq!(
            app.status().unwrap().proxy_address,
            format!("0.0.0.0:{next_port}")
        );
        tokio::net::TcpStream::connect(("127.0.0.1", next_port))
            .await
            .expect("the restarted wildcard listener should accept loopback connections");
        let codex_config_path = root.join("codex/config.toml");
        let configured_proxy_url = format!("http://127.0.0.1:{next_port}/codex/v1");
        let stale_proxy_url = format!("http://0.0.0.0:{next_port}/codex/v1");
        let configured = fs::read_to_string(&codex_config_path).unwrap();
        assert!(configured.contains(&configured_proxy_url));

        fs::write(
            &codex_config_path,
            configured.replace(&configured_proxy_url, &stale_proxy_url),
        )
        .unwrap();
        app.reconcile_proxy_configurations().unwrap();
        let reconciled = fs::read_to_string(&codex_config_path).unwrap();
        assert!(reconciled.contains(&configured_proxy_url));
        assert!(!reconciled.contains(&stale_proxy_url));

        app.set_mode(ClientKind::Codex, ConnectionMode::Direct)
            .await
            .unwrap();
        assert!(app.proxy_enabled());
        assert!(
            fs::read_to_string(root.join("codex/auth.json"))
                .unwrap()
                .contains("\"OPENAI_API_KEY\": \"codex-secret\"")
        );
        app.set_mode(ClientKind::Claude, ConnectionMode::Proxy)
            .await
            .unwrap();
        app.update_settings(SettingsPatch {
            language: None,
            proxy_host: None,
            proxy_port: None,
            proxy_enabled: Some(false),
            clients: None,
            client_auth: None,
        })
        .await
        .unwrap();
        assert!(!app.proxy_enabled());
        assert_eq!(
            app.db.client_state(ClientKind::Codex).unwrap().mode,
            ConnectionMode::Direct
        );
        assert_eq!(
            app.db.client_state(ClientKind::Claude).unwrap().mode,
            ConnectionMode::Direct
        );

        app.notify_shutdown();
        proxy.await.unwrap().unwrap();
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
}
