//! Domain types and business rules shared by the hsin CLI and daemon.
//!
//! This crate intentionally contains no storage, operating-system, or transport
//! code. In particular, [`Provider`] never carries provider credentials.

use std::{collections::BTreeMap, error::Error, fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize};
use url::Url;

/// Wire protocol version implemented by this workspace.
pub const PROTOCOL_VERSION: u32 = 1;
/// Monotonic CLI/daemon release compatibility code. Every published workspace
/// version must be exactly one greater than the preceding release so a new CLI
/// always replaces an older daemon.
pub const VERSION_CODE: u32 = 26;

pub const HSIN_CODEX_CONFIG_NAME: &str = "hsin";
pub const OPENAI_CODEX_CONFIG_NAME: &str = "OpenAI";
pub const DEFAULT_CODEX_CONFIG_NAME: &str = OPENAI_CODEX_CONFIG_NAME;

#[must_use]
pub fn provider_name_from_url(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    let host = url.host_str()?.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    // `host_str` keeps the URL bracket syntax around an IPv6 literal. The
    // brackets belong to the URL, not to the address, and nothing rebuilds a
    // URL from this name: the base URL is stored separately. Stripping them
    // first also lets the IP check below actually recognize IPv6, instead of
    // relying on such a host happening to contain no dot.
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
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

const fn default_true() -> bool {
    true
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

/// How daemon-owned outbound HTTP clients reach an upstream provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProxyMode {
    #[default]
    Direct,
    System,
    Manual,
}

impl UpstreamProxyMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::System => "system",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for UpstreamProxyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UpstreamProxyMode {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "direct" => Ok(Self::Direct),
            "system" => Ok(Self::System),
            "manual" => Ok(Self::Manual),
            _ => Err(ParseEnumError::new("upstream proxy mode", value)),
        }
    }
}

/// A provider can inherit the daemon-wide outbound proxy or override it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProxyMode {
    #[default]
    Inherit,
    Direct,
    System,
    Manual,
}

impl ProviderProxyMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Direct => "direct",
            Self::System => "system",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for ProviderProxyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderProxyMode {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "inherit" => Ok(Self::Inherit),
            "direct" => Ok(Self::Direct),
            "system" => Ok(Self::System),
            "manual" => Ok(Self::Manual),
            _ => Err(ParseEnumError::new("provider proxy mode", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyProtocol {
    #[default]
    Http,
    Socks5,
}

impl ProxyProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        }
    }
}

impl fmt::Display for ProxyProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProxyProtocol {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "socks5" | "socks5h" => Ok(Self::Socks5),
            _ => Err(ParseEnumError::new("proxy protocol", value)),
        }
    }
}

/// Public manual-proxy metadata. The password itself is encrypted separately by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualProxyConfig {
    #[serde(default)]
    pub protocol: ProxyProtocol,
    #[serde(default = "default_manual_proxy_host")]
    pub host: String,
    #[serde(default = "default_manual_proxy_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password_configured: bool,
}

impl Default for ManualProxyConfig {
    fn default() -> Self {
        Self {
            protocol: ProxyProtocol::Http,
            host: default_manual_proxy_host(),
            port: default_manual_proxy_port(),
            username: String::new(),
            password_configured: false,
        }
    }
}

impl ManualProxyConfig {
    /// Validate values used to construct a proxy URL without accepting embedded credentials.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the host, port, username, or password-presence metadata is
    /// not safe to use as a manual proxy endpoint.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err(ValidationError::new("network_proxy.host", "empty"));
        }
        if host.chars().count() > 253 || host.chars().any(char::is_whitespace) {
            return Err(ValidationError::new("network_proxy.host", "invalid"));
        }
        let authority_host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let url = Url::parse(&format!("http://{authority_host}:{}", self.port))
            .map_err(|_| ValidationError::new("network_proxy.host", "invalid"))?;
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ValidationError::new("network_proxy.host", "invalid"));
        }
        if self.port == 0 {
            return Err(ValidationError::new("network_proxy.port", "invalid"));
        }
        if self.username.chars().count() > 256 || self.username.chars().any(char::is_control) {
            return Err(ValidationError::new("network_proxy.username", "invalid"));
        }
        if self.password_configured && self.username.trim().is_empty() {
            return Err(ValidationError::new(
                "network_proxy.username",
                "required_for_password",
            ));
        }
        Ok(())
    }
}

fn default_manual_proxy_host() -> String {
    "127.0.0.1".into()
}

const fn default_manual_proxy_port() -> u16 {
    7890
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamProxyConfig {
    #[serde(default)]
    pub mode: UpstreamProxyMode,
    #[serde(default)]
    pub manual: ManualProxyConfig,
}

impl UpstreamProxyConfig {
    /// Validate the active daemon-wide outbound proxy mode.
    ///
    /// # Errors
    ///
    /// Returns a validation error when manual mode is active and its endpoint is invalid.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.mode == UpstreamProxyMode::Manual {
            self.manual.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProxyConfig {
    #[serde(default)]
    pub mode: ProviderProxyMode,
    #[serde(default)]
    pub manual: ManualProxyConfig,
}

impl ProviderProxyConfig {
    /// Validate a provider's outbound proxy override.
    ///
    /// # Errors
    ///
    /// Returns a validation error when manual override mode is active and its endpoint is invalid.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.mode == ProviderProxyMode::Manual {
            self.manual.validate()?;
        }
        Ok(())
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
    /// The session default written to `ANTHROPIC_MODEL`.
    ///
    /// Claude Code resolves the startup model as `--model` > `ANTHROPIC_MODEL` > the selection
    /// persisted in `settings.json`. Without this key a stale persisted selection — a first-party
    /// model ID the upstream provider has never heard of — outranks the tier mapping and the
    /// session fails before the mapping is ever consulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_context_1m: bool,
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
        !self.enabled
            || (self.trimmed_default_model().is_none()
                && self.slots().iter().all(|(_, slot)| slot.is_none()))
    }

    fn trimmed_default_model(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
    }

    /// The session default written to `ANTHROPIC_MODEL`, including its optional 1M suffix.
    #[must_use]
    pub fn resolved_default_model(&self) -> Option<String> {
        self.trimmed_default_model().map(|model| {
            if self.default_context_1m {
                format!("{model}[1m]")
            } else {
                model.to_owned()
            }
        })
    }

    /// The value to write for one of the [`CLAUDE_MODEL_ENV_KEYS`], or `None` when this mapping
    /// does not own it.
    ///
    /// Each mapped tier drives three keys: the resolved model ID, the label Claude Code shows in
    /// the model picker, and the description under it. Without the label the picker lists the raw
    /// ID including the `[1m]` suffix, which is why the mapped model is otherwise invisible.
    #[must_use]
    pub fn env_value(&self, key: &str) -> Option<String> {
        if key == CLAUDE_MODEL_ENV {
            return self.resolved_default_model();
        }
        CLAUDE_TIER_ENV
            .iter()
            .zip(self.slots())
            .find_map(|(tier, (_, slot))| {
                let slot = slot?;
                let bare = slot.model.trim();
                if bare.is_empty() {
                    return None;
                }
                if key == tier.model {
                    Some(slot.resolved_model())
                } else if key == tier.name {
                    Some(bare.to_owned())
                } else if key == tier.description {
                    Some(format!("{} → {}", tier.label, slot.resolved_model()))
                } else {
                    None
                }
            })
    }

    /// Validate the mapping for a provider bound to `client`.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the mapping is attached to a non-Claude provider, or the
    /// default model or a mapped model ID is empty or oversized.
    pub fn validate(&self, client: ClientKind) -> Result<(), ValidationError> {
        if client != ClientKind::Claude {
            return Err(ValidationError::new(
                "claude_model_mapping",
                "unsupported_client",
            ));
        }
        if let Some(default_model) = self.default_model.as_deref() {
            if default_model.trim().is_empty() {
                return Err(ValidationError::new(CLAUDE_MODEL_ENV, "empty"));
            }
            if default_model.chars().count() > 256 {
                return Err(ValidationError::new(CLAUDE_MODEL_ENV, "too_long"));
            }
        }
        if self.default_context_1m && self.trimmed_default_model().is_none() {
            return Err(ValidationError::new(
                CLAUDE_MODEL_ENV,
                "context_without_model",
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
        Ok(())
    }
}

/// `settings.json` env keys owned by [`ClaudeModelMapping`].
pub const CLAUDE_MODEL_ENV: &str = "ANTHROPIC_MODEL";
pub const CLAUDE_FABLE_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_FABLE_MODEL";
pub const CLAUDE_OPUS_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";
pub const CLAUDE_SONNET_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_SONNET_MODEL";
pub const CLAUDE_HAIKU_MODEL_ENV: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";

/// The three env keys one tier owns, plus the tier's name in Claude Code's own vocabulary.
struct TierEnv {
    label: &'static str,
    model: &'static str,
    name: &'static str,
    description: &'static str,
}

/// Aligned with [`ClaudeModelMapping::slots`].
const CLAUDE_TIER_ENV: [TierEnv; 4] = [
    TierEnv {
        label: "Fable",
        model: CLAUDE_FABLE_MODEL_ENV,
        name: "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
        description: "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    },
    TierEnv {
        label: "Opus",
        model: CLAUDE_OPUS_MODEL_ENV,
        name: "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        description: "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    },
    TierEnv {
        label: "Sonnet",
        model: CLAUDE_SONNET_MODEL_ENV,
        name: "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        description: "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    },
    TierEnv {
        label: "Haiku",
        model: CLAUDE_HAIKU_MODEL_ENV,
        name: "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        description: "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
    },
];

/// Every env key the Claude model mapping may write, in display order.
pub const CLAUDE_MODEL_ENV_KEYS: [&str; 13] = [
    CLAUDE_MODEL_ENV,
    CLAUDE_TIER_ENV[0].model,
    CLAUDE_TIER_ENV[0].name,
    CLAUDE_TIER_ENV[0].description,
    CLAUDE_TIER_ENV[1].model,
    CLAUDE_TIER_ENV[1].name,
    CLAUDE_TIER_ENV[1].description,
    CLAUDE_TIER_ENV[2].model,
    CLAUDE_TIER_ENV[2].name,
    CLAUDE_TIER_ENV[2].description,
    CLAUDE_TIER_ENV[3].model,
    CLAUDE_TIER_ENV[3].name,
    CLAUDE_TIER_ENV[3].description,
];

/// Claude Code picker labels and descriptions controlled by the client-level name-mapping switch.
pub const CLAUDE_MODEL_NAME_ENV_KEYS: [&str; 8] = [
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
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
    pub codex_config_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model_mapping: Option<ClaudeModelMapping>,
    #[serde(default)]
    pub scope: ProviderScope,
    #[serde(default)]
    pub codex_image: CodexImageConfig,
    #[serde(default)]
    pub network_proxy: ProviderProxyConfig,
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
            codex_config_name: self.codex_config_name.clone(),
            claude_model_mapping: self.claude_model_mapping.clone(),
            scope: self.scope,
            codex_image: self.codex_image.clone(),
            network_proxy: self.network_proxy.clone(),
        }
        .validate()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderScope {
    #[default]
    Primary,
    ImageOnly,
}

impl ProviderScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::ImageOnly => "image_only",
        }
    }
}

impl fmt::Display for ProviderScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderScope {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" => Ok(Self::Primary),
            "image_only" | "image-only" => Ok(Self::ImageOnly),
            _ => Err(ParseEnumError::new("provider scope", value)),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexImageConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
}

impl CodexImageConfig {
    #[must_use]
    pub fn is_inert(&self) -> bool {
        !self.enabled && self.models.is_empty() && self.preferred_model.is_none()
    }

    /// Validate and normalize a persisted image model configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for empty, oversized, duplicate, or inconsistent model IDs.
    pub fn normalized(&self) -> Result<Self, ValidationError> {
        if self.models.len() > 32 {
            return Err(ValidationError::new("codex_image.models", "too_many"));
        }
        let mut models = Vec::with_capacity(self.models.len());
        for model in &self.models {
            let model = model.trim();
            if model.is_empty() {
                return Err(ValidationError::new("codex_image.models", "empty"));
            }
            if model.chars().count() > 256 {
                return Err(ValidationError::new("codex_image.models", "too_long"));
            }
            if models.iter().any(|existing: &String| existing == model) {
                continue;
            }
            models.push(model.to_owned());
        }
        let preferred_model = self
            .preferred_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        if self.enabled && models.is_empty() {
            return Err(ValidationError::new("codex_image.models", "empty"));
        }
        if models.is_empty() && preferred_model.is_some() {
            return Err(ValidationError::new(
                "codex_image.preferred_model",
                "without_models",
            ));
        }
        if let Some(preferred) = &preferred_model
            && !models.iter().any(|model| model == preferred)
        {
            return Err(ValidationError::new(
                "codex_image.preferred_model",
                "not_allowed",
            ));
        }
        if self.enabled && preferred_model.is_none() {
            return Err(ValidationError::new("codex_image.preferred_model", "empty"));
        }
        Ok(Self {
            enabled: self.enabled,
            models,
            preferred_model,
        })
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
    pub codex_config_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model_mapping: Option<ClaudeModelMapping>,
    #[serde(default)]
    pub scope: ProviderScope,
    #[serde(default)]
    pub codex_image: CodexImageConfig,
    #[serde(default)]
    pub network_proxy: ProviderProxyConfig,
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

        normalize_codex_config_name(self.client, self.codex_config_name.as_deref())?;

        if let Some(mapping) = &self.claude_model_mapping {
            mapping.validate(self.client)?;
        }
        let image = self.codex_image.normalized()?;
        self.network_proxy.validate()?;
        match self.client {
            ClientKind::Claude if self.scope != ProviderScope::Primary || !image.is_inert() => {
                return Err(ValidationError::new("codex_image", "unsupported_client"));
            }
            ClientKind::Codex
                if self.scope == ProviderScope::ImageOnly && !self.codex_image.enabled =>
            {
                return Err(ValidationError::new("codex_image", "image_only_disabled"));
            }
            _ => {}
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

/// Normalize the provider name written inside `[model_providers.hsin]`.
///
/// Codex currently uses the exact display name `OpenAI` as a provider capability signal. Claude
/// Code has no equivalent field, so carrying a value for Claude is rejected instead of silently
/// ignoring a configuration mistake.
///
/// # Errors
///
/// Returns a validation error when a Codex name is empty or too long, or when a name is supplied
/// for Claude Code.
pub fn normalize_codex_config_name(
    client: ClientKind,
    name: Option<&str>,
) -> Result<Option<String>, ValidationError> {
    match client {
        ClientKind::Claude => {
            if name.is_some() {
                Err(ValidationError::new(
                    "codex_config_name",
                    "unsupported_client",
                ))
            } else {
                Ok(None)
            }
        }
        ClientKind::Codex => {
            let name = name.unwrap_or(DEFAULT_CODEX_CONFIG_NAME).trim();
            if name.is_empty() {
                return Err(ValidationError::new("codex_config_name", "empty"));
            }
            if name.chars().count() > 128 {
                return Err(ValidationError::new("codex_config_name", "too_long"));
            }
            Ok(Some(name.to_owned()))
        }
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
    #[serde(default, skip_serializing_if = "CodexConfigNameUpdate::is_preserve")]
    pub codex_config_name: CodexConfigNameUpdate,
    #[serde(default, skip_serializing_if = "ClaudeModelMappingUpdate::is_preserve")]
    pub claude_model_mapping: ClaudeModelMappingUpdate,
    #[serde(default, skip_serializing_if = "CodexImageConfigUpdate::is_preserve")]
    pub codex_image: CodexImageConfigUpdate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_proxy: Option<ProviderProxyConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CodexImageConfigUpdate {
    #[default]
    Preserve,
    Set(CodexImageConfig),
}

impl CodexImageConfigUpdate {
    #[must_use]
    pub const fn is_preserve(&self) -> bool {
        matches!(self, Self::Preserve)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CodexConfigNameUpdate {
    #[default]
    Preserve,
    Set(String),
}

impl CodexConfigNameUpdate {
    #[must_use]
    pub const fn is_preserve(&self) -> bool {
        matches!(self, Self::Preserve)
    }
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
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SecretInput {
    #[default]
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
    #[serde(default)]
    pub proxy_password: SecretInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEditParams {
    pub id: String,
    pub expected_revision: u64,
    pub patch: ProviderPatch,
    pub secret: SecretInput,
    #[serde(default)]
    pub proxy_password: SecretInput,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexImageListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexImageSwitchParams {
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
    #[serde(default)]
    pub network_proxy: ProviderProxyConfig,
    #[serde(default)]
    pub proxy_password: SecretInput,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_image_active_provider_id: Option<String>,
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
    #[serde(default = "default_true")]
    pub claude_model_names_enabled: bool,
    #[serde(default)]
    pub upstream_proxy: UpstreamProxyConfig,
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
            claude_model_names_enabled: true,
            upstream_proxy: UpstreamProxyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamProxyUpdate {
    pub config: UpstreamProxyConfig,
    #[serde(default)]
    pub password: SecretInput,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model_names_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_proxy: Option<UpstreamProxyUpdate>,
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

impl DoctorReport {
    #[must_use]
    pub fn from_findings(findings: Vec<DoctorFinding>) -> Self {
        Self {
            healthy: !findings
                .iter()
                .any(|item| item.severity == DoctorSeverity::Error),
            findings,
        }
    }
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
        assert_eq!(
            serde_json::to_string(&ProviderScope::ImageOnly).unwrap(),
            "\"image_only\""
        );
        assert_eq!(
            serde_json::to_string(&UpstreamProxyMode::System).unwrap(),
            "\"system\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderProxyMode::Inherit).unwrap(),
            "\"inherit\""
        );
        assert_eq!(
            serde_json::to_string(&ProxyProtocol::Socks5).unwrap(),
            "\"socks5\""
        );
    }

    #[test]
    fn manual_proxy_validation_rejects_unsafe_or_incomplete_values() {
        let mut manual = ManualProxyConfig {
            protocol: ProxyProtocol::Socks5,
            host: "proxy.example.test".into(),
            port: 1080,
            username: "proxy-user".into(),
            password_configured: true,
        };
        assert!(manual.validate().is_ok());

        manual.username.clear();
        assert_eq!(
            manual.validate(),
            Err(ValidationError::new(
                "network_proxy.username",
                "required_for_password"
            ))
        );
        manual.password_configured = false;
        manual.host = "user:password@proxy.example.test".into();
        assert_eq!(
            manual.validate(),
            Err(ValidationError::new("network_proxy.host", "invalid"))
        );
        manual.host = "proxy.example.test/path".into();
        assert_eq!(
            manual.validate(),
            Err(ValidationError::new("network_proxy.host", "invalid"))
        );
    }

    #[test]
    fn public_proxy_configs_expose_only_password_presence() {
        let settings = Settings {
            upstream_proxy: UpstreamProxyConfig {
                mode: UpstreamProxyMode::Manual,
                manual: ManualProxyConfig {
                    username: "proxy-user".into(),
                    password_configured: true,
                    ..ManualProxyConfig::default()
                },
            },
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings).unwrap();
        assert!(encoded.contains("\"password_configured\":true"));
        assert!(!encoded.contains("proxy-password"));
    }

    #[test]
    fn codex_image_models_are_normalized_and_preferred_must_be_allowed() {
        let normalized = CodexImageConfig {
            enabled: false,
            models: vec![
                " gpt-image-2 ".into(),
                "gpt-image-2".into(),
                "image-alpha".into(),
            ],
            preferred_model: Some(" gpt-image-2 ".into()),
        }
        .normalized()
        .unwrap();
        assert!(!normalized.enabled);
        assert_eq!(normalized.models, ["gpt-image-2", "image-alpha"]);
        assert_eq!(normalized.preferred_model.as_deref(), Some("gpt-image-2"));

        assert_eq!(
            CodexImageConfig {
                enabled: true,
                models: vec!["gpt-image-2".into()],
                preferred_model: Some("other".into()),
            }
            .normalized(),
            Err(ValidationError::new(
                "codex_image.preferred_model",
                "not_allowed"
            ))
        );
    }

    #[test]
    fn image_only_providers_are_codex_only_and_must_be_enabled() {
        let mut draft = ProviderDraft {
            client: ClientKind::Codex,
            name: "Images".into(),
            description: String::new(),
            base_url: "https://images.example.test/v1".into(),
            auth_scheme: AuthScheme::Bearer,
            model: None,
            codex_config_name: None,
            claude_model_mapping: None,
            scope: ProviderScope::ImageOnly,
            codex_image: CodexImageConfig::default(),
            network_proxy: ProviderProxyConfig::default(),
        };
        assert_eq!(
            draft.validate(),
            Err(ValidationError::new("codex_image", "image_only_disabled"))
        );
        draft.codex_image = CodexImageConfig {
            enabled: true,
            models: vec!["gpt-image-2".into()],
            preferred_model: Some("gpt-image-2".into()),
        };
        assert!(draft.validate().is_ok());
        draft.client = ClientKind::Claude;
        assert_eq!(
            draft.validate(),
            Err(ValidationError::new("codex_image", "unsupported_client"))
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
    fn claude_model_name_mapping_defaults_on_for_older_settings() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "language": "system",
            "proxy_host": "127.0.0.1",
            "proxy_port": 9999
        }))
        .unwrap();
        assert!(settings.claude_model_names_enabled);
        assert!(Settings::default().claude_model_names_enabled);
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
            codex_config_name: None,
            claude_model_mapping: None,
            scope: ProviderScope::Primary,
            codex_image: CodexImageConfig::default(),
            network_proxy: ProviderProxyConfig::default(),
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
    fn codex_config_names_are_client_scoped_and_normalized() {
        assert_eq!(
            normalize_codex_config_name(ClientKind::Codex, None).unwrap(),
            Some(DEFAULT_CODEX_CONFIG_NAME.into())
        );
        assert_eq!(
            normalize_codex_config_name(ClientKind::Codex, Some(" OpenAI ")).unwrap(),
            Some(OPENAI_CODEX_CONFIG_NAME.into())
        );
        assert_eq!(
            normalize_codex_config_name(ClientKind::Codex, Some(" hsin ")).unwrap(),
            Some(HSIN_CODEX_CONFIG_NAME.into())
        );
        assert_eq!(
            normalize_codex_config_name(ClientKind::Claude, None).unwrap(),
            None
        );
        assert!(normalize_codex_config_name(ClientKind::Codex, Some(" ")).is_err());
        assert!(normalize_codex_config_name(ClientKind::Claude, Some("OpenAI")).is_err());
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

    /// An address is a whole label. Splitting one on dots the way a domain is
    /// split would name the provider after a single octet, and the URL brackets
    /// around an IPv6 literal are syntax rather than part of the address.
    #[test]
    fn provider_names_keep_ip_literals_intact() {
        for (url, expected) in [
            ("https://192.168.1.10/v1", "192.168.1.10"),
            ("http://[::1]:8080/v1", "::1"),
            ("http://[2001:db8::1]/v1", "2001:db8::1"),
            // `url` normalizes an IPv4-mapped literal to hexadecimal, but the
            // name must survive even if it ever stops doing that.
            ("http://[::ffff:192.168.1.1]/v1", "::ffff:c0a8:101"),
        ] {
            assert_eq!(
                provider_name_from_url(url).as_deref(),
                Some(expected),
                "unexpected name for {url}"
            );
        }
    }

    /// A name generated before brackets were stripped still equals the bracketed
    /// host, so editing such a provider migrates it to the bare address.
    #[test]
    fn a_bracketed_ipv6_name_normalizes_to_the_bare_address() {
        assert_eq!(
            normalize_generated_provider_name("[::1]", "http://[::1]:8080/v1"),
            "::1"
        );
        assert_eq!(
            normalize_generated_provider_name("Home Box", "http://[::1]:8080/v1"),
            "Home Box"
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

    #[test]
    fn doctor_health_is_derived_from_error_findings() {
        let warning = DoctorFinding {
            code: "warning".into(),
            severity: DoctorSeverity::Warning,
            args: BTreeMap::new(),
        };
        assert!(DoctorReport::from_findings(vec![warning.clone()]).healthy);

        let error = DoctorFinding {
            code: "error".into(),
            severity: DoctorSeverity::Error,
            args: BTreeMap::new(),
        };
        assert!(!DoctorReport::from_findings(vec![warning, error]).healthy);
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
    fn a_default_model_supports_the_1m_suffix() {
        // ANTHROPIC_MODEL is useful with no tier mapped at all: on its own it stops a stale
        // persisted selection from being sent to a provider that never had that model.
        let mapping = ClaudeModelMapping {
            enabled: true,
            default_model: Some("deepseek-v4-pro".into()),
            default_context_1m: true,
            ..ClaudeModelMapping::default()
        };
        assert!(!mapping.is_inert());
        assert_eq!(
            mapping.env_value(CLAUDE_MODEL_ENV).as_deref(),
            Some("deepseek-v4-pro[1m]")
        );
    }

    #[test]
    fn a_mapped_tier_names_and_describes_itself_for_the_model_picker() {
        // Claude Code labels the picker entry with `_NAME` and falls back to the raw ID, suffix
        // and all, when it is absent — which is what made the mapping invisible in the picker.
        let mapping = ClaudeModelMapping {
            enabled: true,
            opus: Some(slot("deepseek-v4-pro", true)),
            ..ClaudeModelMapping::default()
        };
        assert_eq!(
            mapping.env_value(CLAUDE_OPUS_MODEL_ENV).as_deref(),
            Some("deepseek-v4-pro[1m]")
        );
        assert_eq!(
            mapping
                .env_value("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME")
                .as_deref(),
            Some("deepseek-v4-pro"),
            "the label drops the suffix; it is a name, not a model ID"
        );
        assert_eq!(
            mapping
                .env_value("ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION")
                .as_deref(),
            Some("Opus → deepseek-v4-pro[1m]")
        );
        // An unmapped tier owns none of its three keys, so they are restored rather than written.
        for key in [
            CLAUDE_SONNET_MODEL_ENV,
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
        ] {
            assert_eq!(mapping.env_value(key), None, "{key}");
        }
        // Every key the writer iterates has to be one some tier can claim.
        assert!(CLAUDE_MODEL_ENV_KEYS.contains(&"ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"));
        assert!(CLAUDE_MODEL_ENV_KEYS.contains(&CLAUDE_MODEL_ENV));
        assert_eq!(CLAUDE_MODEL_NAME_ENV_KEYS.len(), 8);
        assert!(
            CLAUDE_MODEL_NAME_ENV_KEYS
                .iter()
                .all(|key| CLAUDE_MODEL_ENV_KEYS.contains(key))
        );
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

        // A tier can map to any upstream model, so support is determined by that model rather than
        // the first-party Claude model used as the tier's placeholder.
        let haiku_1m = ClaudeModelMapping {
            enabled: true,
            haiku: Some(slot("deepseek-flash", true)),
            ..ClaudeModelMapping::default()
        };
        assert!(haiku_1m.validate(ClientKind::Claude).is_ok());
        assert_eq!(
            haiku_1m.env_value(CLAUDE_HAIKU_MODEL_ENV).as_deref(),
            Some("deepseek-flash[1m]")
        );

        let default_1m_without_model = ClaudeModelMapping {
            enabled: true,
            default_context_1m: true,
            opus: Some(slot("claude-opus-5", false)),
            ..ClaudeModelMapping::default()
        };
        assert!(
            default_1m_without_model
                .validate(ClientKind::Claude)
                .is_err()
        );
    }

    #[test]
    fn a_provider_serialized_before_the_mapping_existed_still_deserializes() {
        let legacy = r#"{"id":"p","client":"claude","name":"n","base_url":"https://a.test",
            "auth_scheme":"x_api_key","revision":1}"#;
        let provider: Provider = serde_json::from_str(legacy).expect("legacy provider");
        assert_eq!(provider.claude_model_mapping, None);

        let legacy_codex = r#"{"id":"p","client":"codex","name":"n","base_url":"https://a.test/v1",
            "auth_scheme":"bearer","revision":1}"#;
        let provider: Provider = serde_json::from_str(legacy_codex).expect("legacy Codex provider");
        assert_eq!(provider.codex_config_name, None);

        let draft: ProviderDraft = serde_json::from_str(
            r#"{"client":"codex","name":"n","base_url":"https://a.test/v1","auth_scheme":"bearer"}"#,
        )
        .expect("legacy provider draft");
        assert_eq!(draft.codex_config_name, None);

        let older_mapping: ClaudeModelMapping =
            serde_json::from_str(r#"{"enabled":true,"default_model":"claude-opus-5"}"#)
                .expect("mapping saved before the default 1M option");
        assert!(!older_mapping.default_context_1m);
    }
}
