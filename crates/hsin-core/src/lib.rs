//! Domain types and business rules shared by the hsin CLI and daemon.
//!
//! This crate intentionally contains no storage, operating-system, or transport
//! code. In particular, [`Provider`] never carries provider credentials.

use std::{collections::BTreeMap, error::Error, fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize};
use url::Url;

/// Wire protocol version implemented by this workspace.
pub const PROTOCOL_VERSION: u32 = 1;
/// Monotonic CLI/daemon compatibility code. Bump when either side requires
/// RPC fields or behavior that an older binary cannot provide.
pub const VERSION_CODE: u32 = 22;

#[must_use]
pub fn provider_name_from_url(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    let host = url.host_str()?.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    if host.parse::<IpAddr>().is_ok() || !host.contains('.') {
        return Some(host.to_owned());
    }
    host.rsplit('.').nth(1).map(str::to_owned)
}

#[must_use]
pub fn normalize_generated_provider_name(current: &str, base_url: &str) -> String {
    let current = current.trim();
    let host = Url::parse(base_url.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    if host
        .as_deref()
        .is_some_and(|host| current.eq_ignore_ascii_case(host))
    {
        return provider_name_from_url(base_url).unwrap_or_else(|| current.to_owned());
    }
    current.to_owned()
}

#[must_use]
pub fn convert_provider_base_url(base_url: &str, source: ClientKind, target: ClientKind) -> String {
    if source == target {
        return base_url.to_owned();
    }

    match (source, target) {
        (ClientKind::Codex, ClientKind::Claude) => base_url
            .strip_suffix("/v1/")
            .or_else(|| base_url.strip_suffix("/v1"))
            .unwrap_or(base_url)
            .to_owned(),
        (ClientKind::Claude, ClientKind::Codex) => {
            let trimmed = base_url.trim_end_matches('/');
            if trimmed.ends_with("/v1") {
                trimmed.to_owned()
            } else {
                format!("{trimmed}/v1")
            }
        }
        _ => base_url.to_owned(),
    }
}

/// A stable identifier for an AI client whose provider hsin can manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Codex,
    #[serde(rename = "claude")]
    Claude,
}

impl ClientKind {
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for ClientKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ClientKind {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" | "claude-code" | "claude_code" => Ok(Self::Claude),
            _ => Err(ParseEnumError::new("client", value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSettings {
    #[serde(default = "default_client_order")]
    pub order: Vec<ClientKind>,
    #[serde(default = "default_client_order")]
    pub visible: Vec<ClientKind>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAuthSettings {
    #[serde(default)]
    pub codex_disable_custom_auth: bool,
    #[serde(default)]
    pub claude_disable_custom_auth: bool,
}

impl ClientAuthSettings {
    #[must_use]
    pub const fn disable_custom_auth(self, client: ClientKind) -> bool {
        match client {
            ClientKind::Codex => self.codex_disable_custom_auth,
            ClientKind::Claude => self.claude_disable_custom_auth,
        }
    }

    pub fn set_disable_custom_auth(&mut self, client: ClientKind, disabled: bool) {
        match client {
            ClientKind::Codex => self.codex_disable_custom_auth = disabled,
            ClientKind::Claude => self.claude_disable_custom_auth = disabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAuthUpdate {
    pub client: ClientKind,
    pub disable_custom_auth: bool,
}

impl ClientSettings {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.order.len() == ClientKind::ALL.len()
            && ClientKind::ALL.iter().all(|client| {
                self.order
                    .iter()
                    .filter(|candidate| *candidate == client)
                    .count()
                    == 1
            })
            && !self.visible.is_empty()
            && self.visible.len() <= ClientKind::ALL.len()
            && self.visible.iter().all(|client| {
                self.order.contains(client)
                    && self
                        .visible
                        .iter()
                        .filter(|candidate| *candidate == client)
                        .count()
                        == 1
            })
    }

    #[must_use]
    pub fn visible_in_order(&self) -> Vec<ClientKind> {
        self.order
            .iter()
            .copied()
            .filter(|client| self.visible.contains(client))
            .collect()
    }
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            order: default_client_order(),
            visible: default_client_order(),
        }
    }
}

fn default_client_order() -> Vec<ClientKind> {
    ClientKind::ALL.to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    Direct,
    Proxy,
}

impl ConnectionMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
        }
    }
}

impl fmt::Display for ConnectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ConnectionMode {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "direct" => Ok(Self::Direct),
            "proxy" => Ok(Self::Proxy),
            _ => Err(ParseEnumError::new("connection_mode", value)),
        }
    }
}

/// Authentication header style used by an upstream provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    Bearer,
    XApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

impl AuthScheme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::XApiKey => "x_api_key",
            Self::OAuth => "oauth",
        }
    }
}

impl fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuthScheme {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bearer" => Ok(Self::Bearer),
            "x_api_key" | "x-api-key" | "xapikey" => Ok(Self::XApiKey),
            "oauth" => Ok(Self::OAuth),
            _ => Err(ParseEnumError::new("auth_scheme", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigStatus {
    Unmanaged,
    Synchronized,
    Drifted,
    Conflict,
    Unavailable,
}

/// One Claude Code model tier mapped onto an upstream model ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSlot {
    pub model: String,
    /// Requests the 1M-context variant, written as a `[1m]` suffix on the model ID.
    #[serde(default)]
    pub context_1m: bool,
}

impl ModelSlot {
    /// The value written to `settings.json`, including the `[1m]` suffix when requested.
    #[must_use]
    pub fn resolved_model(&self) -> String {
        let model = self.model.trim();
        if self.context_1m {
            format!("{model}[1m]")
        } else {
            model.to_owned()
        }
    }
}

/// Per-provider mapping of Claude Code's model tiers onto upstream model IDs.
///
/// Only meaningful for [`ClientKind::Claude`]. Tiers left as `None` are not written, and the whole
/// mapping is inert while `enabled` is false.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeModelMapping {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fable: Option<ModelSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus: Option<ModelSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet: Option<ModelSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku: Option<ModelSlot>,
}

impl ClaudeModelMapping {
    /// The tiers in display order, paired with the `settings.json` env key each one owns.
    #[must_use]
    pub fn slots(&self) -> [(&'static str, Option<&ModelSlot>); 4] {
        [
            (CLAUDE_FABLE_MODEL_ENV, self.fable.as_ref()),
            (CLAUDE_OPUS_MODEL_ENV, self.opus.as_ref()),
            (CLAUDE_SONNET_MODEL_ENV, self.sonnet.as_ref()),
            (CLAUDE_HAIKU_MODEL_ENV, self.haiku.as_ref()),
        ]
    }

    /// Whether this mapping writes anything at all.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        !self.enabled || self.slots().iter().all(|(_, slot)| slot.is_none())
    }

    /// Validate the mapping for a provider bound to `client`.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the mapping is attached to a non-Claude provider, a mapped
    /// model ID is empty or oversized, or 1M context is requested for the Haiku tier (which has no
    /// 1M variant upstream).
    pub fn validate(&self, client: ClientKind) -> Result<(), ValidationError> {
        if client != ClientKind::Claude {
            return Err(ValidationError::new(
                "claude_model_mapping",
                "unsupported_client",
            ));
        }
        for (field, slot) in self.slots() {
            let Some(slot) = slot else { continue };
            if slot.model.trim().is_empty() {
                return Err(ValidationError::new(field, "empty"));
            }
            if slot.model.chars().count() > 256 {
                return Err(ValidationError::new(field, "too_long"));
            }
        }
        if self.haiku.as_ref().is_some_and(|slot| slot.context_1m) {
            return Err(ValidationError::new(
                CLAUDE_HAIKU_MODEL_ENV,
                "context_1m_unsupported",
            ));
        }
        Ok(())
    }
}

/// `settings.json` env keys owned by [`ClaudeModelMapping`].
pub const CLAUDE_FABLE_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_FABLE_MODEL";
pub const CLAUDE_OPUS_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";
pub const CLAUDE_SONNET_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_SONNET_MODEL";
pub const CLAUDE_HAIKU_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";

/// Every env key the Claude model mapping may write, in display order.
pub const CLAUDE_MODEL_ENV_KEYS: [&str; 4] = [
    CLAUDE_FABLE_MODEL_ENV,
    CLAUDE_OPUS_MODEL_ENV,
    CLAUDE_SONNET_MODEL_ENV,
    CLAUDE_HAIKU_MODEL_ENV,
];

/// Public provider metadata. Credentials are deliberately stored separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub client: ClientKind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub credential_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model_mapping: Option<ClaudeModelMapping>,
    pub revision: u64,
}

impl Provider {
    /// Validate untrusted provider data at a process boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the ID or provider draft is invalid.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.id.trim().is_empty() {
            return Err(ValidationError::new("id", "empty"));
        }
        ProviderDraft {
            client: self.client,
            name: self.name.clone(),
            description: self.description.clone(),
            base_url: self.base_url.clone(),
            auth_scheme: self.auth_scheme,
            model: self.model.clone(),
            claude_model_mapping: self.claude_model_mapping.clone(),
        }
        .validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDraft {
    pub client: ClientKind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model_mapping: Option<ClaudeModelMapping>,
}

impl ProviderDraft {
    /// Validate a provider name and upstream HTTP(S) URL.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for an empty/oversized name, malformed URL,
    /// embedded URL credentials, a non-HTTP scheme, or a URL fragment.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(ValidationError::new("name", "empty"));
        }
        if name.chars().count() > 128 {
            return Err(ValidationError::new("name", "too_long"));
        }
        if self.description.chars().count() > 1024 {
            return Err(ValidationError::new("description", "too_long"));
        }
        if let Some(model) = &self.model {
            if model.trim().is_empty() {
                return Err(ValidationError::new("model", "empty"));
            }
            if model.chars().count() > 256 {
                return Err(ValidationError::new("model", "too_long"));
            }
        }

        if let Some(mapping) = &self.claude_model_mapping {
            mapping.validate(self.client)?;
        }

        let url = Url::parse(self.base_url.trim())
            .map_err(|_| ValidationError::new("base_url", "invalid_url"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ValidationError::new("base_url", "unsupported_scheme"));
        }
        if url.host_str().is_none() {
            return Err(ValidationError::new("base_url", "missing_host"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ValidationError::new("base_url", "embedded_credentials"));
        }
        if url.fragment().is_some() {
            return Err(ValidationError::new("base_url", "fragment_not_allowed"));
        }
        if url.query().is_some() {
            return Err(ValidationError::new("base_url", "query_not_allowed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<AuthScheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "ModelUpdate::is_preserve")]
    pub model: ModelUpdate,
    #[serde(default, skip_serializing_if = "ClaudeModelMappingUpdate::is_preserve")]
    pub claude_model_mapping: ClaudeModelMappingUpdate,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ClaudeModelMappingUpdate {
    #[default]
    Preserve,
    Set(ClaudeModelMapping),
    Clear,
}

impl ClaudeModelMappingUpdate {
    #[must_use]
    pub const fn is_preserve(&self) -> bool {
        matches!(self, Self::Preserve)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ModelUpdate {
    #[default]
    Preserve,
    Set(String),
    Clear,
}

impl ModelUpdate {
    #[must_use]
    pub const fn is_preserve(&self) -> bool {
        matches!(self, Self::Preserve)
    }
}

/// How an RPC should update an existing secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SecretInput {
    Preserve,
    Replace(String),
    Clear,
}

impl fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preserve => f.write_str("Preserve"),
            Self::Replace(_) => f.write_str("Replace([REDACTED])"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAddParams {
    pub provider: ProviderDraft,
    pub secret: SecretInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEditParams {
    pub id: String,
    pub expected_revision: u64,
    pub patch: ProviderPatch,
    pub secret: SecretInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRemoveParams {
    pub id: String,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSwitchParams {
    pub client: ClientKind,
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDiscoverParams {
    pub client: ClientKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
    pub secret: SecretInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDiscovery {
    pub models: Vec<String>,
    pub resolved_base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCurrentParams {
    pub client: ClientKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCurrentResult {
    pub provider: Provider,
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSetParams {
    pub client: ClientKind,
    pub mode: ConnectionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientState {
    pub client: ClientKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider_id: Option<String>,
    pub mode: ConnectionMode,
    pub config_status: ConfigStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub locked: bool,
    pub proxy_listening: bool,
    #[serde(default)]
    pub proxy_enabled: bool,
    pub proxy_address: String,
    pub clients: Vec<ClientState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub language: String,
    pub proxy_host: String,
    pub proxy_port: u16,
    #[serde(default)]
    pub proxy_enabled: bool,
    #[serde(default)]
    pub clients: ClientSettings,
    #[serde(default)]
    pub client_auth: ClientAuthSettings,
}

pub const LANGUAGE_SYSTEM: &str = "system";
pub const LANGUAGE_EN_US: &str = "en-US";
pub const LANGUAGE_ZH_CN: &str = "zh-CN";

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: LANGUAGE_SYSTEM.into(),
            proxy_host: "127.0.0.1".into(),
            proxy_port: 9999,
            proxy_enabled: false,
            clients: ClientSettings::default(),
            client_auth: ClientAuthSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<ClientSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_auth: Option<ClientAuthUpdate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStoreState {
    Unlocked,
    Locked,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityStatus {
    pub key_store: KeyStoreState,
    pub key_version: u32,
    pub recovery_key_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorFinding {
    pub code: String,
    pub severity: DoctorSeverity,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub findings: Vec<DoctorFinding>,
}

/// Stable, localizable application error codes shared by all frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    RevisionConflict,
    ConfigConflict,
    ConfigUnavailable,
    KeyStoreLocked,
    KeyStoreUnavailable,
    AuthenticationFailed,
    PermissionDenied,
    ProtocolMismatch,
    FrameTooLarge,
    Timeout,
    DaemonUnavailable,
    OAuthProxyUnsupported,
    NoActiveProvider,
    CurrentCredentialUnavailable,
    Internal,
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::RevisionConflict => "revision_conflict",
            Self::ConfigConflict => "config_conflict",
            Self::ConfigUnavailable => "config_unavailable",
            Self::KeyStoreLocked => "key_store_locked",
            Self::KeyStoreUnavailable => "key_store_unavailable",
            Self::AuthenticationFailed => "authentication_failed",
            Self::PermissionDenied => "permission_denied",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::FrameTooLarge => "frame_too_large",
            Self::Timeout => "timeout",
            Self::DaemonUnavailable => "daemon_unavailable",
            Self::OAuthProxyUnsupported => "oauth_proxy_unsupported",
            Self::NoActiveProvider => "no_active_provider",
            Self::CurrentCredentialUnavailable => "current_credential_unavailable",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error payload suitable for localization at the presentation layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppError {
    pub code: ErrorCode,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub retryable: bool,
}

impl AppError {
    #[must_use]
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            args: BTreeMap::new(),
            retryable: false,
        }
    }

    #[must_use]
    pub fn with_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub const fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code)
    }
}

impl Error for AppError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEnumError {
    kind: &'static str,
    value: String,
}

impl ParseEnumError {
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

impl fmt::Display for ParseEnumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown {} value {:?}", self.kind, self.value)
    }
}

impl Error for ParseEnumError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: &'static str,
    pub reason: &'static str,
}

impl ValidationError {
    #[must_use]
    pub const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.field, self.reason)
    }
}

impl Error for ValidationError {}

impl From<ValidationError> for AppError {
    fn from(value: ValidationError) -> Self {
        Self::new(ErrorCode::InvalidArgument)
            .with_arg("field", value.field)
            .with_arg("reason", value.reason)
    }
}

/// Serialize a value to a JSON object, useful for assembling error arguments.
///
/// # Errors
///
/// Returns [`AppError`] if serialization fails or the value is not an object.
pub fn json_object<T: Serialize>(
    value: T,
) -> Result<BTreeMap<String, serde_json::Value>, AppError> {
    match serde_json::to_value(value).map_err(|_| AppError::new(ErrorCode::Internal))? {
        serde_json::Value::Object(object) => Ok(object.into_iter().collect()),
        _ => {
            Err(AppError::new(ErrorCode::InvalidArgument)
                .with_arg("reason", "expected_json_object"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_have_stable_wire_and_cli_names() {
        assert_eq!(ClientKind::from_str("claude-code"), Ok(ClientKind::Claude));
        assert_eq!(ConnectionMode::from_str("PROXY"), Ok(ConnectionMode::Proxy));
        assert_eq!(AuthScheme::from_str("x-api-key"), Ok(AuthScheme::XApiKey));
        assert_eq!(AuthScheme::from_str("oauth"), Ok(AuthScheme::OAuth));
        assert_eq!(
            serde_json::to_string(&ClientKind::Claude).unwrap(),
            "\"claude\""
        );
        assert_eq!(
            serde_json::to_string(&AuthScheme::XApiKey).unwrap(),
            "\"x_api_key\""
        );
        assert_eq!(
            serde_json::to_string(&AuthScheme::OAuth).unwrap(),
            "\"oauth\""
        );
    }

    #[test]
    fn client_settings_require_complete_order_and_one_visible_client() {
        let default = ClientSettings::default();
        assert!(default.is_valid());
        assert_eq!(default.visible_in_order(), ClientKind::ALL);

        let reordered = ClientSettings {
            order: vec![ClientKind::Claude, ClientKind::Codex],
            visible: vec![ClientKind::Codex, ClientKind::Claude],
        };
        assert!(reordered.is_valid());
        assert_eq!(
            reordered.visible_in_order(),
            [ClientKind::Claude, ClientKind::Codex]
        );

        assert!(
            !ClientSettings {
                order: ClientKind::ALL.to_vec(),
                visible: Vec::new(),
            }
            .is_valid()
        );
        assert!(
            !ClientSettings {
                order: vec![ClientKind::Codex, ClientKind::Codex],
                visible: vec![ClientKind::Codex],
            }
            .is_valid()
        );
    }

    #[test]
    fn provider_validation_rejects_unsafe_urls() {
        let valid = ProviderDraft {
            client: ClientKind::Codex,
            name: "Example".into(),
            description: String::new(),
            base_url: "https://api.example.com/v1".into(),
            auth_scheme: AuthScheme::Bearer,
            model: None,
            claude_model_mapping: None,
        };
        assert!(valid.validate().is_ok());

        let mut invalid = valid.clone();
        invalid.base_url = "https://token@example.com/v1".into();
        assert_eq!(
            invalid.validate(),
            Err(ValidationError::new("base_url", "embedded_credentials"))
        );

        invalid.base_url = "file:///tmp/upstream".into();
        assert_eq!(
            invalid.validate(),
            Err(ValidationError::new("base_url", "unsupported_scheme"))
        );

        invalid.base_url = "https://api.example.com/v1?tenant=other".into();
        assert_eq!(
            invalid.validate(),
            Err(ValidationError::new("base_url", "query_not_allowed"))
        );
    }

    #[test]
    fn provider_names_use_the_label_before_the_top_level_domain() {
        assert_eq!(
            provider_name_from_url("https://ai.router.team/v1").as_deref(),
            Some("router")
        );
        assert_eq!(
            provider_name_from_url("https://api.example.com/v1").as_deref(),
            Some("example")
        );
        assert_eq!(
            provider_name_from_url("http://127.0.0.1:8080/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            normalize_generated_provider_name("ai.router.team", "https://ai.router.team/v1"),
            "router"
        );
        assert_eq!(
            normalize_generated_provider_name("My Router", "https://ai.router.team/v1"),
            "My Router"
        );
    }

    #[test]
    fn provider_base_urls_convert_between_supported_clients() {
        assert_eq!(
            convert_provider_base_url(
                "https://api.example.com/v1",
                ClientKind::Codex,
                ClientKind::Claude
            ),
            "https://api.example.com"
        );
        assert_eq!(
            convert_provider_base_url(
                "https://api.example.com",
                ClientKind::Claude,
                ClientKind::Codex
            ),
            "https://api.example.com/v1"
        );
        assert_eq!(
            convert_provider_base_url(
                "https://api.example.com/custom",
                ClientKind::Codex,
                ClientKind::Claude
            ),
            "https://api.example.com/custom"
        );
        assert_eq!(
            convert_provider_base_url(
                "https://api.example.com/v1",
                ClientKind::Codex,
                ClientKind::Codex
            ),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let secret = SecretInput::Replace("never-print-me".into());
        assert_eq!(format!("{secret:?}"), "Replace([REDACTED])");
        assert!(!format!("{secret:?}").contains("never-print-me"));
    }

    #[test]
    fn app_error_uses_stable_shape() {
        let error = AppError::new(ErrorCode::RevisionConflict)
            .with_arg("expected", "3")
            .retryable(true);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "code": "revision_conflict",
                "args": {"expected": "3"},
                "retryable": true
            })
        );
    }
}

#[cfg(test)]
mod claude_model_mapping_tests {
    use super::*;

    fn slot(model: &str, context_1m: bool) -> ModelSlot {
        ModelSlot {
            model: model.into(),
            context_1m,
        }
    }

    #[test]
    fn the_1m_option_is_written_as_a_model_suffix() {
        assert_eq!(
            ModelSlot {
                model: "claude-opus-5".into(),
                context_1m: true,
            }
            .resolved_model(),
            "claude-opus-5[1m]"
        );
        assert_eq!(
            ModelSlot {
                model: " claude-opus-5 ".into(),
                context_1m: false,
            }
            .resolved_model(),
            "claude-opus-5"
        );
    }

    #[test]
    fn a_mapping_is_inert_unless_enabled_with_at_least_one_tier() {
        let mut mapping = ClaudeModelMapping::default();
        assert!(mapping.is_inert());
        mapping.enabled = true;
        assert!(mapping.is_inert(), "enabled but empty writes nothing");
        mapping.opus = Some(slot("claude-opus-5", false));
        assert!(!mapping.is_inert());
        mapping.enabled = false;
        assert!(mapping.is_inert(), "the master switch overrides the tiers");
    }

    #[test]
    fn mapping_validation_rejects_non_claude_providers_and_bad_models() {
        let mapping = ClaudeModelMapping {
            enabled: true,
            opus: Some(slot("claude-opus-5", true)),
            ..ClaudeModelMapping::default()
        };
        assert!(mapping.validate(ClientKind::Claude).is_ok());
        // The mapping writes Claude Code settings, so it is meaningless on a Codex provider.
        assert!(mapping.validate(ClientKind::Codex).is_err());

        let empty = ClaudeModelMapping {
            enabled: true,
            opus: Some(slot("   ", false)),
            ..ClaudeModelMapping::default()
        };
        assert!(empty.validate(ClientKind::Claude).is_err());

        // claude-haiku-4-5 has no 1M-context variant upstream.
        let haiku_1m = ClaudeModelMapping {
            enabled: true,
            haiku: Some(slot("claude-haiku-4-5", true)),
            ..ClaudeModelMapping::default()
        };
        assert!(haiku_1m.validate(ClientKind::Claude).is_err());
    }

    #[test]
    fn a_provider_serialized_before_the_mapping_existed_still_deserializes() {
        let legacy = r#"{"id":"p","client":"claude","name":"n","base_url":"https://a.test",
            "auth_scheme":"x_api_key","revision":1}"#;
        let provider: Provider = serde_json::from_str(legacy).expect("legacy provider");
        assert_eq!(provider.claude_model_mapping, None);
    }
}
