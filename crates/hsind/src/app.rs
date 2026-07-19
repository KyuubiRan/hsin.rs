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
    AppError, AuthScheme, ClientKind, ClientSettings, ConnectionMode, DaemonStatus, DoctorFinding,
    DoctorReport, DoctorSeverity, ErrorCode, ImportCurrentParams, ImportCurrentResult,
    KeyStoreState, ModelDiscoverParams, ModelDiscovery, ModelUpdate, Provider, ProviderAddParams,
    ProviderEditParams, ProviderListParams, ProviderRemoveParams, ProviderSwitchParams,
    SecretInput, SecurityStatus, Settings, SettingsPatch,
};
use parking_lot::RwLock;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, watch};
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
    proxy_enabled: watch::Sender<bool>,
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
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(4))
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .map_err(|error| DaemonError::Internal(error.to_string()))?;
        let enabled = db
            .setting("proxy_enabled")?
            .is_some_and(|value| value == "true");
        let (proxy_enabled, _) = watch::channel(enabled);
        Ok(Arc::new(Self {
            db,
            crypto,
            mutation: Mutex::new(()),
            credential_command,
            proxy_listening: AtomicBool::new(false),
            proxy_enabled,
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

    pub fn mark_proxy_listening(&self, listening: bool) {
        self.proxy_listening.store(listening, Ordering::Release);
    }
    pub fn proxy_enabled(&self) -> bool {
        *self.proxy_enabled.borrow()
    }
    pub fn subscribe_proxy_enabled(&self) -> watch::Receiver<bool> {
        self.proxy_enabled.subscribe()
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
        let target = ConfigTarget {
            client,
            mode: state.mode,
            provider: provider.clone(),
            credential_command: self.credential_command.to_string_lossy().into_owned(),
            proxy_port: self.proxy_port()?,
        };
        Ok((config::patch_text(&current, &target)? == current).then_some(provider))
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
        let name = params.patch.name.unwrap_or(current.name);
        let input = ProviderInput {
            client: current.client,
            name,
            description: params.patch.description.unwrap_or(current.description),
            base_url: params.patch.base_url.unwrap_or(current.base_url),
            auth_scheme: params.patch.auth_scheme.unwrap_or(current.auth_scheme),
            model,
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
            let target = ConfigTarget {
                client: provider.client,
                mode: state.mode,
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
            self.db
                .set_active(params.client, &provider.id, "synchronized")?;
            self.db.set_mode(params.client, ConnectionMode::Direct)?;
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
        let mut target: ConfigTarget = serde_json::from_str(target_json)?;
        let persisted = self.db.get_provider(&target.provider.id)?;
        target.provider.official = persisted.official;
        target.provider.credential_configured = persisted.credential_configured;
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
            proxy_enabled: self.proxy_enabled(),
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
                .unwrap_or_else(|| hsin_core::LANGUAGE_SYSTEM.into()),
            proxy_host: "127.0.0.1".into(),
            proxy_port: self.proxy_port()?,
            proxy_enabled: self.proxy_enabled(),
            clients: self.client_settings()?,
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

    pub async fn update_settings(&self, patch: SettingsPatch) -> Result<Settings> {
        let _guard = self.mutation.lock().await;
        if patch
            .clients
            .as_ref()
            .is_some_and(|clients| !clients.is_valid())
        {
            return Err(DaemonError::Invalid(
                "client settings must contain each client once and keep at least one visible"
                    .into(),
            ));
        }
        if let Some(language) = patch.language {
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
        if patch.proxy_enabled == Some(false) {
            self.disable_all_client_proxies_locked()?;
            self.set_proxy_enabled_locked(false).await?;
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
        if let Some(clients) = patch.clients {
            self.db
                .set_setting("clients", &serde_json::to_string(&clients)?)?;
        }
        if patch.proxy_enabled == Some(true) {
            self.set_proxy_enabled_locked(true).await?;
        }
        self.settings()
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
            self.proxy_enabled.send_replace(enabled);
        }
        for _ in 0..50 {
            if self.proxy_listening.load(Ordering::Acquire) == enabled {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if enabled {
            self.db.set_setting("proxy_enabled", "false")?;
            self.proxy_enabled.send_replace(false);
            return Err(DaemonError::Config(
                "local proxy did not become ready".into(),
            ));
        }
        Err(DaemonError::Config(
            "local proxy did not stop in time".into(),
        ))
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
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(clients.clone()),
            })
            .await
            .unwrap();
        assert_eq!(settings.clients, clients);
        assert_eq!(app.settings().unwrap().clients, clients);

        let invalid = app
            .update_settings(SettingsPatch {
                language: None,
                proxy_port: None,
                proxy_enabled: None,
                clients: Some(ClientSettings {
                    order: ClientKind::ALL.to_vec(),
                    visible: Vec::new(),
                }),
            })
            .await;
        assert!(matches!(invalid, Err(DaemonError::Invalid(_))));
        assert_eq!(app.settings().unwrap().clients, clients);
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
        app.db.set_setting("proxy_port", &port.to_string()).unwrap();

        let codex = app
            .add_provider(ProviderAddParams {
                provider: hsin_core::ProviderDraft {
                    client: ClientKind::Codex,
                    name: "Codex custom".into(),
                    description: String::new(),
                    base_url: "https://codex.example.test/v1".into(),
                    auth_scheme: AuthScheme::Bearer,
                    model: None,
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

        app.set_mode(ClientKind::Codex, ConnectionMode::Direct)
            .await
            .unwrap();
        assert!(app.proxy_enabled());
        app.set_mode(ClientKind::Claude, ConnectionMode::Proxy)
            .await
            .unwrap();
        app.update_settings(SettingsPatch {
            language: None,
            proxy_port: None,
            proxy_enabled: Some(false),
            clients: None,
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
