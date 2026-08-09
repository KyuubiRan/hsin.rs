use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::fs::File;

use atomic_write_file::AtomicWriteFile;
use directories::BaseDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, ImDocument, Item, Table, value};
use zeroize::{Zeroize, Zeroizing};

use hsin_core::CLAUDE_MODEL_ENV_KEYS;

use crate::{
    error::{DaemonError, Result},
    model::{AuthScheme, ClientKind, ConnectionMode, Provider},
};

pub const CODEX_OFFICIAL_URL: &str = "https://api.openai.com/v1";
pub const CLAUDE_OFFICIAL_URL: &str = "https://api.anthropic.com";
pub const HSIN_MANAGED_KEY: &str = "HSIN_MANAGED_KEY";

pub struct DetectedProvider {
    pub name: String,
    pub base_url: String,
    pub auth_scheme: AuthScheme,
    pub secret: Option<Zeroizing<String>>,
    pub official: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTarget {
    pub client: ClientKind,
    pub mode: ConnectionMode,
    pub provider: Provider,
    pub credential_command: String,
    #[serde(default = "default_proxy_host")]
    pub proxy_host: String,
    pub proxy_port: u16,
    #[serde(default)]
    pub disable_custom_auth: bool,
    #[serde(default)]
    pub codex_auth_before_hash: Option<String>,
    /// Values the user had for the model-mapping env keys before hsin first took them over.
    /// Restored whenever a tier is not mapped, so disabling the mapping is non-destructive.
    #[serde(default)]
    pub claude_model_env_before: Option<ClaudeModelEnvSnapshot>,
}

/// The user's own values for the env keys owned by [`ClaudeModelMapping`], captured once before
/// hsin first writes them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeModelEnvSnapshot {
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// The keys this snapshot was taken for. Snapshots written before hsin owned more than the
    /// four tier keys carry no list, so they default to exactly those four: everything else in the
    /// file is still the user's own and has to be captured before hsin first overwrites it.
    #[serde(default = "legacy_covered_keys")]
    pub covered: BTreeSet<String>,
}

fn legacy_covered_keys() -> BTreeSet<String> {
    [
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

impl ClaudeModelEnvSnapshot {
    /// Read the current values of the mapped env keys out of a Claude `settings.json`.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Config`] when the text is not parseable JSONC.
    pub fn capture(text: &str) -> Result<Self> {
        let value = if text.trim().is_empty() {
            serde_json::json!({})
        } else {
            jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                .map_err(|error| DaemonError::Config(error.to_string()))?
                .unwrap_or_else(|| serde_json::json!({}))
        };
        let env = value.get("env");
        let mut values = BTreeMap::new();
        for key in CLAUDE_MODEL_ENV_KEYS {
            if let Some(existing) = env
                .and_then(|env| env.get(key))
                .and_then(serde_json::Value::as_str)
                .filter(|existing| !existing.trim().is_empty())
            {
                values.insert(key.to_owned(), existing.to_owned());
            }
        }
        Ok(Self {
            values,
            covered: CLAUDE_MODEL_ENV_KEYS
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
        })
    }

    /// Whether every key hsin currently owns has already been captured.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        CLAUDE_MODEL_ENV_KEYS
            .iter()
            .all(|key| self.covered.contains(*key))
    }

    /// Capture the keys this snapshot does not cover yet from the live file.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Config`] when the text is not parseable JSONC.
    pub fn extend_uncovered(&mut self, text: &str) -> Result<()> {
        let fresh = Self::capture(text)?;
        for key in CLAUDE_MODEL_ENV_KEYS {
            if self.covered.insert(key.to_owned())
                && let Some(value) = fresh.get(key)
            {
                self.values.insert(key.to_owned(), value.to_owned());
            }
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

fn default_proxy_host() -> String {
    "127.0.0.1".into()
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct CodexAuthSnapshot {
    pub auth_path: String,
    pub file_existed: bool,
    pub auth_mode: Option<String>,
    pub openai_api_key: Option<String>,
}

/// Resolve a client's configuration path.
///
/// `home_override` wins over the environment so a daemon launched by a service
/// definition that cannot carry environment variables still reaches the same
/// files the installing session did.
pub fn default_config_path(client: ClientKind, home_override: Option<&Path>) -> Result<PathBuf> {
    let home = BaseDirs::new()
        .ok_or_else(|| DaemonError::Config("cannot resolve the user home directory".into()))?
        .home_dir()
        .to_path_buf();
    let client_home = home_override.map(PathBuf::from).or_else(|| {
        std::env::var_os(match client {
            ClientKind::Codex => "CODEX_HOME",
            ClientKind::Claude => "CLAUDE_CONFIG_DIR",
        })
        .map(PathBuf::from)
    });
    Ok(match client {
        ClientKind::Codex => client_home
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        ClientKind::Claude => client_home
            .unwrap_or_else(|| home.join(".claude"))
            .join("settings.json"),
    })
}

pub fn codex_auth_path(config_path: &Path) -> Result<PathBuf> {
    config_path
        .parent()
        .map(|parent| parent.join("auth.json"))
        .ok_or_else(|| DaemonError::Config("Codex config path has no parent directory".into()))
}

pub fn detect_current(path: &Path, client: ClientKind) -> Result<DetectedProvider> {
    let text = if path.exists() {
        fs::read_to_string(path).map_err(|error| {
            DaemonError::Config(format!("cannot read {}: {error}", path.display()))
        })?
    } else {
        String::new()
    };
    match client {
        ClientKind::Codex => {
            let auth_path = codex_auth_path(path)?;
            let auth_text = if auth_path.exists() {
                fs::read_to_string(&auth_path).map_err(|error| {
                    DaemonError::Config(format!("cannot read {}: {error}", auth_path.display()))
                })?
            } else {
                String::new()
            };
            detect_codex(&text, &auth_text)
        }
        ClientKind::Claude => detect_claude(&text),
    }
}

fn detect_codex(text: &str, auth_text: &str) -> Result<DetectedProvider> {
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| DaemonError::Config(error.to_string()))?;
    let provider_id = document
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or("openai");
    if provider_id == "openai" {
        let base_url = document
            .get("openai_base_url")
            .and_then(Item::as_str)
            .unwrap_or(CODEX_OFFICIAL_URL)
            .trim_end_matches('/')
            .to_owned();
        if same_url(&base_url, CODEX_OFFICIAL_URL) {
            return Ok(official_provider(ClientKind::Codex));
        }
        return Ok(DetectedProvider {
            name: imported_name("OpenAI", &base_url),
            base_url,
            auth_scheme: AuthScheme::Bearer,
            secret: std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(Zeroizing::new),
            official: false,
        });
    }

    let provider = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(provider_id))
        .and_then(Item::as_table)
        .ok_or_else(|| {
            DaemonError::Config(format!("active Codex provider {provider_id} is missing"))
        })?;
    let base_url = provider
        .get("base_url")
        .and_then(Item::as_str)
        .ok_or_else(|| DaemonError::Config("active Codex provider has no base_url".into()))?
        .trim_end_matches('/')
        .to_owned();
    if same_url(&base_url, CODEX_OFFICIAL_URL)
        && provider
            .get("requires_openai_auth")
            .and_then(Item::as_bool)
            .unwrap_or(false)
    {
        return Ok(official_provider(ClientKind::Codex));
    }
    let name = provider
        .get("name")
        .and_then(Item::as_str)
        .map_or_else(|| imported_name(provider_id, &base_url), str::to_owned);
    let requires_openai_auth = provider
        .get("requires_openai_auth")
        .and_then(Item::as_bool)
        .unwrap_or(false);
    let auth_secret = if requires_openai_auth {
        codex_auth_api_key(auth_text)?
            .filter(|value| is_importable_secret(value))
            .map(Zeroizing::new)
    } else {
        None
    };
    let secret = provider
        .get("experimental_bearer_token")
        .and_then(Item::as_str)
        .filter(|value| is_importable_secret(value))
        .map(|value| Zeroizing::new(value.to_owned()))
        .or_else(|| {
            provider
                .get("env_key")
                .and_then(Item::as_str)
                .and_then(|key| std::env::var(key).ok())
                .filter(|value| is_importable_secret(value))
                .map(Zeroizing::new)
        })
        .or(auth_secret);
    Ok(DetectedProvider {
        name,
        base_url,
        auth_scheme: AuthScheme::Bearer,
        secret,
        official: false,
    })
}

fn detect_claude(text: &str) -> Result<DetectedProvider> {
    let value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
            .map_err(|error| DaemonError::Config(error.to_string()))?
            .ok_or_else(|| DaemonError::Config("empty Claude settings".into()))?
    };
    let env = value.get("env");
    let base_url = env
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(CLAUDE_OFFICIAL_URL)
        .trim_end_matches('/')
        .to_owned();
    let auth_token_value = env
        .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let api_key_value = env
        .and_then(|env| env.get("ANTHROPIC_API_KEY"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let api_key_helper = value
        .get("apiKeyHelper")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    if same_url(&base_url, CLAUDE_OFFICIAL_URL)
        && auth_token_value.is_none()
        && api_key_value.is_none()
        && api_key_helper.is_none()
    {
        return Ok(official_provider(ClientKind::Claude));
    }
    let auth_token = auth_token_value.filter(|value| is_importable_secret(value));
    let api_key = api_key_value.filter(|value| is_importable_secret(value));
    Ok(DetectedProvider {
        name: imported_name("Claude", &base_url),
        base_url,
        auth_scheme: if auth_token_value.is_some() {
            AuthScheme::Bearer
        } else {
            AuthScheme::XApiKey
        },
        secret: auth_token
            .or(api_key)
            .map(|value| Zeroizing::new(value.to_owned())),
        official: false,
    })
}

pub fn official_provider(client: ClientKind) -> DetectedProvider {
    let base_url = match client {
        ClientKind::Codex => CODEX_OFFICIAL_URL,
        ClientKind::Claude => CLAUDE_OFFICIAL_URL,
    };
    DetectedProvider {
        name: "Official".into(),
        base_url: base_url.into(),
        auth_scheme: AuthScheme::OAuth,
        secret: None,
        official: true,
    }
}

fn imported_name(fallback: &str, base_url: &str) -> String {
    hsin_core::provider_name_from_url(base_url).unwrap_or_else(|| fallback.to_owned())
}

fn same_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/').eq_ignore_ascii_case(right)
}

fn is_importable_secret(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != HSIN_MANAGED_KEY
}

#[cfg(test)]
pub fn apply(path: &Path, expected_hash: Option<&str>, target: &ConfigTarget) -> Result<String> {
    apply_with_credential(path, expected_hash, target, None)
}

pub fn apply_with_credential(
    path: &Path,
    expected_hash: Option<&str>,
    target: &ConfigTarget,
    credential: Option<&str>,
) -> Result<String> {
    atomic_patch(path, expected_hash, |before| {
        patch_text_with_credential(before, target, credential)
    })
}

pub fn patch_text_with_credential(
    before: &str,
    target: &ConfigTarget,
    credential: Option<&str>,
) -> Result<String> {
    match target.client {
        ClientKind::Codex => patch_codex_with_credential(before, target, credential),
        ClientKind::Claude => patch_claude_with_credential(before, target, credential),
    }
}

pub fn file_hash(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(hash(&fs::read(path)?)))
}

pub fn capture_codex_auth(path: &Path) -> Result<CodexAuthSnapshot> {
    let file_existed = path.exists();
    let text = if file_existed {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let value = parse_json_object(&text, "Codex auth")?;
    Ok(CodexAuthSnapshot {
        auth_path: path.to_string_lossy().into_owned(),
        file_existed,
        auth_mode: optional_string(&value, "auth_mode", "Codex auth")?,
        openai_api_key: optional_string(&value, "OPENAI_API_KEY", "Codex auth")?,
    })
}

pub fn apply_codex_auth(path: &Path, expected_hash: Option<&str>, api_key: &str) -> Result<String> {
    atomic_patch(path, expected_hash, |before| {
        patch_codex_auth_text(before, api_key)
    })
}

pub fn patch_codex_auth_text(before: &str, api_key: &str) -> Result<String> {
    let mut output = if before.trim().is_empty() {
        "{}\n".to_owned()
    } else {
        before.to_owned()
    };
    validate_jsonc(&output)?;
    output = set_root_string(&output, "auth_mode", Some("apikey"))?;
    output = set_root_string(&output, "OPENAI_API_KEY", Some(api_key))?;
    validate_jsonc(&output)?;
    Ok(output)
}

pub fn restore_codex_auth(
    path: &Path,
    expected_hash: Option<&str>,
    snapshot: &CodexAuthSnapshot,
) -> Result<String> {
    atomic_patch(path, expected_hash, |before| {
        restore_codex_auth_text(before, snapshot)
    })
}

pub fn restore_codex_auth_text(before: &str, snapshot: &CodexAuthSnapshot) -> Result<String> {
    let mut output = if before.trim().is_empty() {
        "{}\n".to_owned()
    } else {
        before.to_owned()
    };
    validate_jsonc(&output)?;
    output = set_root_string(&output, "auth_mode", snapshot.auth_mode.as_deref())?;
    output = set_root_string(
        &output,
        "OPENAI_API_KEY",
        snapshot.openai_api_key.as_deref(),
    )?;
    validate_jsonc(&output)?;
    Ok(output)
}

pub fn codex_auth_is_managed(text: &str, api_key: &str) -> Result<bool> {
    let value = parse_json_object(text, "Codex auth")?;
    Ok(
        optional_string(&value, "auth_mode", "Codex auth")?.as_deref() == Some("apikey")
            && optional_string(&value, "OPENAI_API_KEY", "Codex auth")?.as_deref() == Some(api_key),
    )
}

#[cfg(test)]
pub fn patch_codex(text: &str, target: &ConfigTarget) -> Result<String> {
    patch_codex_with_credential(text, target, None)
}

pub fn patch_codex_with_credential(
    text: &str,
    target: &ConfigTarget,
    credential: Option<&str>,
) -> Result<String> {
    let mut output = text.to_owned();
    if let Some(model) = target.provider.model.as_deref() {
        let document = parse_toml(&output)?;
        if let Some(existing) = document.get("model") {
            let span = existing
                .span()
                .ok_or_else(|| DaemonError::Config("model has no source span".into()))?;
            output.replace_range(span, &value(model).to_string());
        } else {
            let insertion = top_level_property_insertion(
                &document,
                &output,
                &format!("model = {}", value(model)),
            );
            output.insert_str(insertion.offset, &insertion.text);
        }
    }
    let document = parse_toml(&output)?;
    if let Some(existing) = document.get("model_provider") {
        let span = existing
            .span()
            .ok_or_else(|| DaemonError::Config("model_provider has no source span".into()))?;
        output.replace_range(span, "\"hsin\"");
    } else {
        let insertion = top_level_insertion(&document, &output);
        output.insert_str(insertion.offset, &insertion.text);
    }

    let document = parse_toml(&output)?;
    let provider_block = codex_provider_block(target, credential, newline(&output))?;
    match document.get("model_providers") {
        Some(Item::Table(providers)) => match providers.get("hsin") {
            Some(Item::Value(existing)) => {
                let span = existing.span().ok_or_else(|| {
                    DaemonError::Config("hsin provider has no source span".into())
                })?;
                output.replace_range(span, &codex_provider_inline(target, credential)?);
            }
            Some(existing) => {
                let mut ranges = Vec::new();
                collect_explicit_table_ranges(existing, &output, &mut ranges);
                let ranges = merge_whitespace_separated_ranges(ranges, &output);
                let Some(first) = ranges.first().cloned() else {
                    return Err(DaemonError::Config(
                        "dotted hsin provider tables are not supported".into(),
                    ));
                };
                for range in ranges.iter().skip(1).rev() {
                    output.replace_range(range.clone(), "");
                }
                output.replace_range(first, &provider_block);
            }
            None => append_toml_table(&mut output, &provider_block),
        },
        Some(_) => {
            return Err(DaemonError::Config(
                "model_providers must be a TOML table".into(),
            ));
        }
        None => append_toml_table(&mut output, &provider_block),
    }
    parse_toml(&output)?;
    Ok(output)
}

fn parse_toml(text: &str) -> Result<ImDocument<String>> {
    ImDocument::parse(text.to_owned()).map_err(|error| DaemonError::Config(error.to_string()))
}

fn codex_provider_table(target: &ConfigTarget, credential: Option<&str>) -> Result<Table> {
    let base_url = match target.mode {
        ConnectionMode::Direct => target.provider.base_url.trim_end_matches('/').to_owned(),
        ConnectionMode::Proxy => proxy_url(target, "/codex/v1")?,
    };
    let mut provider = Table::new();
    provider.set_implicit(false);
    provider["name"] = value("hsin");
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    if target.provider.official {
        provider["requires_openai_auth"] = value(true);
        return Ok(provider);
    }
    if target.disable_custom_auth {
        let _ = configured_key(target, credential)?;
        provider["requires_openai_auth"] = value(true);
        return Ok(provider);
    }
    let mut auth = Table::new();
    auth["command"] = value(target.credential_command.clone());
    let args = match target.mode {
        ConnectionMode::Direct => vec![
            "credential".to_owned(),
            "codex".to_owned(),
            "--provider-id".to_owned(),
            target.provider.id.clone(),
            "--revision".to_owned(),
            target.provider.revision.to_string(),
        ],
        ConnectionMode::Proxy => vec!["credential".to_owned(), "codex".to_owned()],
    };
    auth["args"] = toml_edit::value(toml_edit::Array::from_iter(args));
    auth["timeout_ms"] = value(5000);
    auth["refresh_interval_ms"] = value(0);
    provider.insert("auth", Item::Table(auth));
    Ok(provider)
}

fn codex_provider_block(
    target: &ConfigTarget,
    credential: Option<&str>,
    newline: &str,
) -> Result<String> {
    let mut document = DocumentMut::new();
    let mut providers = Table::new();
    providers.set_implicit(true);
    providers.insert(
        "hsin",
        Item::Table(codex_provider_table(target, credential)?),
    );
    document["model_providers"] = Item::Table(providers);
    Ok(document.to_string().replace('\n', newline))
}

fn codex_provider_inline(target: &ConfigTarget, credential: Option<&str>) -> Result<String> {
    Item::Table(codex_provider_table(target, credential)?)
        .into_value()
        .map(|value| value.to_string())
        .map_err(|_| DaemonError::Config("cannot encode inline hsin provider".into()))
}

struct TextInsertion {
    offset: usize,
    text: String,
}

fn top_level_insertion(document: &ImDocument<String>, text: &str) -> TextInsertion {
    top_level_property_insertion(document, text, "model_provider = \"hsin\"")
}

fn top_level_property_insertion(
    document: &ImDocument<String>,
    text: &str,
    property: &str,
) -> TextInsertion {
    let mut ranges = Vec::new();
    for (_, item) in document.iter() {
        collect_table_starts(item, &mut ranges);
    }
    let offset = ranges.into_iter().min().unwrap_or(text.len());
    let newline = newline(text);
    let leading = if offset > 0 && !text[..offset].ends_with(['\n', '\r']) {
        newline
    } else {
        ""
    };
    TextInsertion {
        offset,
        text: format!("{leading}{property}{newline}"),
    }
}

fn collect_table_starts(item: &Item, starts: &mut Vec<usize>) {
    match item {
        Item::Table(table) => {
            if let Some(span) = table.span() {
                starts.push(span.start);
            }
            for (_, child) in table {
                collect_table_starts(child, starts);
            }
        }
        Item::ArrayOfTables(tables) => {
            if let Some(span) = tables.span() {
                starts.push(span.start);
            }
        }
        Item::None | Item::Value(_) => {}
    }
}

fn collect_explicit_table_ranges(item: &Item, text: &str, ranges: &mut Vec<Range<usize>>) {
    match item {
        Item::Table(table) => {
            if let Some(span) = table.span() {
                ranges.push(extend_to_line_end(span, text));
            }
            for (_, child) in table {
                collect_explicit_table_ranges(child, text, ranges);
            }
        }
        Item::ArrayOfTables(tables) => {
            if let Some(span) = tables.span() {
                ranges.push(extend_to_line_end(span, text));
            }
        }
        Item::None | Item::Value(_) => {}
    }
}

fn extend_to_line_end(mut range: Range<usize>, text: &str) -> Range<usize> {
    while range.end < text.len() && text.as_bytes()[range.end] != b'\n' {
        range.end += 1;
    }
    if range.end < text.len() {
        range.end += 1;
    }
    range
}

fn merge_whitespace_separated_ranges(
    mut ranges: Vec<Range<usize>>,
    text: &str,
) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && previous.end <= range.start
            && text[previous.end..range.start].trim().is_empty()
        {
            previous.end = range.end;
        } else {
            merged.push(range);
        }
    }
    merged
}

fn append_toml_table(output: &mut String, table: &str) {
    let newline = newline(output);
    if !output.is_empty() && !output.ends_with(['\n', '\r']) {
        output.push_str(newline);
    }
    if !output.is_empty() && !output.ends_with(&format!("{newline}{newline}")) {
        output.push_str(newline);
    }
    output.push_str(table);
}

#[cfg(test)]
pub fn patch_claude(text: &str, target: &ConfigTarget) -> Result<String> {
    patch_claude_with_credential(text, target, None)
}

pub fn patch_claude_with_credential(
    text: &str,
    target: &ConfigTarget,
    credential: Option<&str>,
) -> Result<String> {
    let output = patch_claude_credentials(text, target, credential)?;
    let output = patch_claude_model_mapping(&output, target)?;
    validate_jsonc(&output)?;
    Ok(output)
}

/// Write the per-provider model mapping into `env`.
///
/// Every key is written on every apply: a mapped tier gets the provider's model ID, an unmapped
/// tier is restored to the value the user had before hsin took the key over (or removed when they
/// had none). That keeps switching providers, and disabling the mapping, non-destructive.
fn patch_claude_model_mapping(text: &str, target: &ConfigTarget) -> Result<String> {
    let mapping = target
        .provider
        .claude_model_mapping
        .as_ref()
        .filter(|mapping| !target.provider.official && mapping.enabled);
    let snapshot = target.claude_model_env_before.clone().unwrap_or_default();
    let mut output = text.to_owned();
    for key in CLAUDE_MODEL_ENV_KEYS {
        let mapped = mapping.and_then(|mapping| mapping.env_value(key));
        let value = mapped.as_deref().or_else(|| snapshot.get(key));
        output = set_nested_string(&output, "env", key, value)?;
    }
    Ok(output)
}

fn patch_claude_credentials(
    text: &str,
    target: &ConfigTarget,
    credential: Option<&str>,
) -> Result<String> {
    let mut output = if text.trim().is_empty() {
        "{}\n".to_owned()
    } else {
        text.to_owned()
    };
    validate_jsonc(&output)?;
    if target.provider.official {
        output = set_nested_string(&output, "env", "ANTHROPIC_BASE_URL", None)?;
        output = set_nested_string(&output, "env", "ANTHROPIC_API_KEY", None)?;
        output = set_nested_string(&output, "env", "ANTHROPIC_AUTH_TOKEN", None)?;
        output = set_root_string(&output, "apiKeyHelper", None)?;
        validate_jsonc(&output)?;
        return Ok(output);
    }
    let base_url = match target.mode {
        ConnectionMode::Direct => target.provider.base_url.trim_end_matches('/').to_owned(),
        ConnectionMode::Proxy => proxy_url(target, "/claude")?,
    };
    output = set_nested_string(&output, "env", "ANTHROPIC_BASE_URL", Some(&base_url))?;
    if target.disable_custom_auth {
        let key = configured_key(target, credential)?;
        let (api_key, auth_token) = match target.mode {
            ConnectionMode::Proxy => (Some(key), None),
            ConnectionMode::Direct => match target.provider.auth_scheme {
                AuthScheme::Bearer => (None, Some(key)),
                AuthScheme::XApiKey => (Some(key), None),
                AuthScheme::OAuth => (None, None),
            },
        };
        output = set_nested_string(&output, "env", "ANTHROPIC_API_KEY", api_key)?;
        output = set_nested_string(&output, "env", "ANTHROPIC_AUTH_TOKEN", auth_token)?;
        output = set_root_string(&output, "apiKeyHelper", None)?;
        validate_jsonc(&output)?;
        return Ok(output);
    }
    output = set_nested_string(&output, "env", "ANTHROPIC_API_KEY", None)?;
    output = set_nested_string(&output, "env", "ANTHROPIC_AUTH_TOKEN", None)?;
    output = set_root_string(
        &output,
        "apiKeyHelper",
        Some(&credential_helper_command(target)),
    )?;
    validate_jsonc(&output)?;
    Ok(output)
}

fn proxy_url(target: &ConfigTarget, path: &str) -> Result<String> {
    let host = target
        .proxy_host
        .parse::<std::net::IpAddr>()
        .map_err(|_| DaemonError::Config("invalid proxy host in configuration target".into()))?;
    let authority = match host {
        std::net::IpAddr::V4(host) => host.to_string(),
        std::net::IpAddr::V6(host) => format!("[{host}]"),
    };
    Ok(format!("http://{authority}:{}{path}", target.proxy_port))
}

fn configured_key<'a>(target: &ConfigTarget, credential: Option<&'a str>) -> Result<&'a str> {
    match target.mode {
        ConnectionMode::Proxy => Ok(HSIN_MANAGED_KEY),
        ConnectionMode::Direct => credential.ok_or(DaemonError::Locked),
    }
}

fn codex_auth_api_key(text: &str) -> Result<Option<String>> {
    let value = parse_json_object(text, "Codex auth")?;
    optional_string(&value, "OPENAI_API_KEY", "Codex auth")
}

fn parse_json_object(
    text: &str,
    label: &str,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
            .map_err(|error| DaemonError::Config(error.to_string()))?
            .ok_or_else(|| DaemonError::Config(format!("empty {label}")))?
    };
    value
        .as_object()
        .cloned()
        .ok_or_else(|| DaemonError::Config(format!("{label} must be a JSON object")))
}

fn optional_string(
    value: &serde_json::Map<String, serde_json::Value>,
    property: &str,
    label: &str,
) -> Result<Option<String>> {
    match value.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(DaemonError::Config(format!(
            "{label} field {property} must be a string"
        ))),
    }
}

fn credential_helper_command(target: &ConfigTarget) -> String {
    let executable = &target.credential_command;
    let binding = match target.mode {
        ConnectionMode::Direct => format!(
            " --provider-id {} --revision {}",
            shell_word(&target.provider.id),
            target.provider.revision
        ),
        ConnectionMode::Proxy => String::new(),
    };
    if cfg!(windows) {
        format!(
            "\"{}\" credential claude{binding}",
            executable.replace('"', "\"\"")
        )
    } else {
        format!("{} credential claude{binding}", shell_word(executable))
    }
}

fn shell_word(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn atomic_patch(
    path: &Path,
    expected_hash: Option<&str>,
    updater: impl FnOnce(&str) -> Result<String>,
) -> Result<String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension(format!(
        "{}.hsin.lock",
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or("config")
    ));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let existed = path.exists();
    let before_bytes = if existed { fs::read(path)? } else { Vec::new() };
    let before_hash = hash(&before_bytes);
    let actual_hash = existed.then_some(before_hash.as_str());
    if actual_hash != expected_hash {
        return Err(DaemonError::Conflict(
            "configuration changed outside hsin".into(),
        ));
    }
    let before = String::from_utf8(before_bytes)
        .map_err(|_| DaemonError::Config("configuration is not UTF-8".into()))?;
    let after = updater(&before)?;
    if after == before {
        return Ok(before_hash);
    }
    let mut output = AtomicWriteFile::open(path)?;
    output.write_all(after.as_bytes())?;
    let latest_exists = path.exists();
    let latest_hash = if latest_exists {
        Some(hash(&fs::read(path)?))
    } else {
        None
    };
    if latest_exists != existed || latest_hash.as_deref() != actual_hash {
        return Err(DaemonError::Conflict(
            "configuration changed while hsin was writing".into(),
        ));
    }
    output.commit()?;
    #[cfg(unix)]
    sync_parent_directory(path)?;
    #[cfg(not(unix))]
    sync_parent_directory(path);
    FileExt::unlock(&lock)?;
    Ok(hash(after.as_bytes()))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Copy)]
struct ObjectRange {
    start: usize,
    end: usize,
}
#[derive(Debug, Clone, Copy)]
struct PropertyRange {
    start: usize,
    value_start: usize,
    value_end: usize,
}

fn validate_jsonc(text: &str) -> Result<()> {
    jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
        .map_err(|error| DaemonError::Config(error.to_string()))?;
    Ok(())
}

fn set_root_string(text: &str, property: &str, value: Option<&str>) -> Result<String> {
    set_object_raw(text, root_object(text)?, property, value.map(quote_json))
}

fn set_nested_string(
    text: &str,
    object_property: &str,
    property: &str,
    value: Option<&str>,
) -> Result<String> {
    let root = root_object(text)?;
    let existing = find_direct_property(text, root, object_property)?;
    if let Some(existing) = existing {
        let start = skip_trivia(text, existing.value_start)?;
        if text.as_bytes().get(start) != Some(&b'{') {
            return Err(DaemonError::Config(format!(
                "{object_property} must be an object"
            )));
        }
        return set_object_raw(
            text,
            ObjectRange {
                start,
                end: find_matching(text, start, b'{', b'}')?,
            },
            property,
            value.map(quote_json),
        );
    }
    if let Some(value) = value {
        let newline = newline(text);
        let raw = format!(
            "{{{newline}    {}: {}{newline}  }}",
            quote_json(property),
            quote_json(value)
        );
        set_object_raw(text, root, object_property, Some(raw))
    } else {
        Ok(text.to_owned())
    }
}

fn set_object_raw(
    text: &str,
    range: ObjectRange,
    property: &str,
    raw: Option<String>,
) -> Result<String> {
    if let Some(existing) = find_direct_property(text, range, property)? {
        if let Some(raw) = raw {
            let output = format!(
                "{}{}{}",
                &text[..existing.value_start],
                raw,
                &text[existing.value_end..]
            );
            return repair_owned_property_layout(&output, range.start, property);
        }
        return remove_property(text, range, existing);
    }
    let Some(raw) = raw else {
        return Ok(text.to_owned());
    };
    let indent = child_indent(text, range);
    let newline = newline(text);
    let interior = &text[range.start + 1..range.end];
    if interior.trim().is_empty() {
        let insertion = format!(
            "{newline}{indent}{}: {raw}{newline}{}",
            quote_json(property),
            parent_indent(&indent)
        );
        return Ok(format!(
            "{}{}{}",
            &text[..=range.start],
            insertion,
            &text[range.end..]
        ));
    }

    let tail = object_tail(text, range)?;
    let closing_indent = closing_indent_start(text, range);
    let insertion_offset = closing_indent.unwrap_or(range.end);
    let insertion = if closing_indent.is_some() {
        format!("{indent}{}: {raw}{newline}", quote_json(property))
    } else {
        format!(
            "{newline}{indent}{}: {raw}{newline}{}",
            quote_json(property),
            parent_indent(&indent)
        )
    };
    let mut output = String::with_capacity(text.len() + insertion.len() + 1);
    if let Some((value_end, has_trailing_comma)) = tail {
        if value_end > insertion_offset {
            return Err(DaemonError::Config("invalid JSONC object layout".into()));
        }
        output.push_str(&text[..value_end]);
        if !has_trailing_comma {
            output.push(',');
        }
        output.push_str(&text[value_end..insertion_offset]);
    } else {
        output.push_str(&text[..insertion_offset]);
    }
    output.push_str(&insertion);
    output.push_str(&text[insertion_offset..]);
    Ok(output)
}

fn remove_property(text: &str, range: ObjectRange, property: PropertyRange) -> Result<String> {
    let bytes = text.as_bytes();
    let line_start = text[..property.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let property_is_indented = line_start > range.start
        && text[line_start..property.start]
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t'));
    if property_is_indented {
        let mut after = property.value_end;
        while matches!(bytes.get(after), Some(b' ' | b'\t')) {
            after += 1;
        }
        let has_following_comma = bytes.get(after) == Some(&b',');
        if has_following_comma {
            after += 1;
            while matches!(bytes.get(after), Some(b' ' | b'\t')) {
                after += 1;
            }
        }
        let line_end = match bytes.get(after) {
            Some(b'\r') if bytes.get(after + 1) == Some(&b'\n') => Some(after + 2),
            Some(b'\n') => Some(after + 1),
            _ => None,
        };
        if let Some(line_end) = line_end {
            if has_following_comma {
                return Ok(format!("{}{}", &text[..line_start], &text[line_end..]));
            }
            let mut before = line_start;
            while before > range.start + 1 && bytes[before - 1].is_ascii_whitespace() {
                before -= 1;
            }
            if before > range.start + 1 && bytes[before - 1] == b',' {
                return Ok(format!(
                    "{}{}{}",
                    &text[..before - 1],
                    &text[before..line_start],
                    &text[line_end..]
                ));
            }
            return Ok(format!("{}{}", &text[..line_start], &text[line_end..]));
        }
    }

    let mut before = property.start;
    while before > range.start + 1 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before > range.start + 1 && bytes[before - 1] == b',' {
        return Ok(format!(
            "{}{}",
            &text[..before - 1],
            &text[property.value_end..]
        ));
    }
    let mut after = skip_trivia(text, property.value_end)?;
    if after < range.end && bytes[after] == b',' {
        after += 1;
    }
    Ok(format!("{}{}", &text[..property.start], &text[after..]))
}

fn root_object(text: &str) -> Result<ObjectRange> {
    let start = skip_trivia(text, 0)?;
    if text.as_bytes().get(start) != Some(&b'{') {
        return Err(DaemonError::Config("JSONC root must be an object".into()));
    }
    Ok(ObjectRange {
        start,
        end: find_matching(text, start, b'{', b'}')?,
    })
}

fn find_direct_property(
    text: &str,
    range: ObjectRange,
    name: &str,
) -> Result<Option<PropertyRange>> {
    let mut index = range.start + 1;
    while index < range.end {
        index = skip_trivia(text, index)?;
        if text.as_bytes().get(index) == Some(&b',') {
            index += 1;
            continue;
        }
        if index >= range.end {
            break;
        }
        if text.as_bytes().get(index) != Some(&b'"') {
            return Err(DaemonError::Config("invalid JSONC object property".into()));
        }
        let property_start = index;
        let string_end = read_string_end(text, index)?;
        let property_name: String = serde_json::from_str(&text[index..string_end])?;
        index = skip_trivia(text, string_end)?;
        if text.as_bytes().get(index) != Some(&b':') {
            return Err(DaemonError::Config(
                "invalid JSONC property separator".into(),
            ));
        }
        let value_start = skip_trivia(text, index + 1)?;
        let value_end = read_value_end(text, value_start)?;
        if property_name == name {
            return Ok(Some(PropertyRange {
                start: property_start,
                value_start,
                value_end,
            }));
        }
        index = value_end;
    }
    Ok(None)
}

fn object_tail(text: &str, range: ObjectRange) -> Result<Option<(usize, bool)>> {
    let mut index = range.start + 1;
    let mut last_value_end = None;
    while index < range.end {
        index = skip_trivia(text, index)?;
        if text.as_bytes().get(index) == Some(&b',') {
            index += 1;
            continue;
        }
        if index >= range.end {
            break;
        }
        if text.as_bytes().get(index) != Some(&b'"') {
            return Err(DaemonError::Config("invalid JSONC object property".into()));
        }
        let string_end = read_string_end(text, index)?;
        index = skip_trivia(text, string_end)?;
        if text.as_bytes().get(index) != Some(&b':') {
            return Err(DaemonError::Config(
                "invalid JSONC property separator".into(),
            ));
        }
        let value_start = skip_trivia(text, index + 1)?;
        let value_end = read_value_end(text, value_start)?;
        last_value_end = Some(value_end);
        index = value_end;
    }
    let Some(value_end) = last_value_end else {
        return Ok(None);
    };
    Ok(Some((
        value_end,
        text.as_bytes().get(skip_trivia(text, value_end)?) == Some(&b','),
    )))
}

fn closing_indent_start(text: &str, range: ObjectRange) -> Option<usize> {
    let line_start = text[..range.end].rfind('\n').map_or(0, |index| index + 1);
    (line_start > range.start
        && text[line_start..range.end]
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t')))
    .then_some(line_start)
}

fn repair_owned_property_layout(text: &str, object_start: usize, property: &str) -> Result<String> {
    let mut output = text.to_owned();

    let mut range = ObjectRange {
        start: object_start,
        end: find_matching(&output, object_start, b'{', b'}')?,
    };
    let mut existing = find_direct_property(&output, range, property)?.ok_or_else(|| {
        DaemonError::Config(format!(
            "JSONC property {property} disappeared during update"
        ))
    })?;
    let leading = &output[range.start + 1..existing.start];
    if leading.trim().is_empty() && leading.bytes().filter(|byte| *byte == b'\n').count() > 1 {
        let replacement = format!("{}{}", newline(&output), child_indent(&output, range));
        output.replace_range(range.start + 1..existing.start, &replacement);
        range.end = find_matching(&output, object_start, b'{', b'}')?;
        existing = find_direct_property(&output, range, property)?.ok_or_else(|| {
            DaemonError::Config(format!(
                "JSONC property {property} disappeared during repair"
            ))
        })?;
    }

    let bytes = output.as_bytes();
    let mut comma = existing.value_end;
    while comma < range.end && bytes[comma].is_ascii_whitespace() {
        comma += 1;
    }
    if comma > existing.value_end
        && output[existing.value_end..comma].contains('\n')
        && bytes.get(comma) == Some(&b',')
    {
        let mut next = comma + 1;
        while next < range.end && bytes[next].is_ascii_whitespace() {
            next += 1;
        }
        if bytes.get(next) == Some(&b'"') {
            let line_start = output[..next].rfind('\n').map_or(next, |index| index + 1);
            let indent = if output[line_start..next]
                .bytes()
                .all(|byte| matches!(byte, b' ' | b'\t'))
            {
                output[line_start..next].to_owned()
            } else {
                child_indent(&output, range)
            };
            let replacement = format!(",{}{indent}", newline(&output));
            output.replace_range(existing.value_end..next, &replacement);
            range.end = find_matching(&output, object_start, b'{', b'}')?;
            existing = find_direct_property(&output, range, property)?.ok_or_else(|| {
                DaemonError::Config(format!(
                    "JSONC property {property} disappeared after separator repair"
                ))
            })?;
        }
    }

    if existing.value_end == range.end && output[range.start..range.end].contains('\n') {
        let closing_indent = parent_indent(&child_indent(&output, range));
        output.insert_str(range.end, &format!("{}{closing_indent}", newline(&output)));
    }
    Ok(output)
}

fn read_value_end(text: &str, start: usize) -> Result<usize> {
    match text.as_bytes().get(start).copied() {
        Some(b'"') => read_string_end(text, start),
        Some(b'{') => Ok(find_matching(text, start, b'{', b'}')? + 1),
        Some(b'[') => Ok(find_matching(text, start, b'[', b']')? + 1),
        Some(_) => {
            let mut index = start;
            while index < text.len() && !matches!(text.as_bytes()[index], b',' | b'}' | b']') {
                index += 1;
            }
            while index > start && text.as_bytes()[index - 1].is_ascii_whitespace() {
                index -= 1;
            }
            Ok(index)
        }
        None => Err(DaemonError::Config("missing JSONC value".into())),
    }
}

fn read_string_end(text: &str, start: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
        } else if bytes[index] == b'"' {
            return Ok(index + 1);
        } else {
            index += 1;
        }
    }
    Err(DaemonError::Config("unterminated JSONC string".into()))
}

fn find_matching(text: &str, start: usize, open: u8, close: u8) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            index = read_string_end(text, index)?;
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index += 2;
            continue;
        }
        if bytes[index] == open {
            depth += 1;
        } else if bytes[index] == close {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        }
        index += 1;
    }
    Err(DaemonError::Config("unterminated JSONC container".into()))
}

fn skip_trivia(text: &str, mut index: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 >= bytes.len() {
                return Err(DaemonError::Config("unterminated JSONC comment".into()));
            }
            index += 2;
            continue;
        }
        return Ok(index);
    }
}

fn child_indent(text: &str, range: ObjectRange) -> String {
    let parent_line = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let parent = &text[parent_line..range.start];
    let first = skip_trivia(text, range.start + 1).unwrap_or(range.end);
    if first < range.end {
        let line = text[..first].rfind('\n').map_or(0, |i| i + 1);
        if line > range.start {
            return text[line..first].to_owned();
        }
    }
    format!("{parent}  ")
}
fn parent_indent(child: &str) -> String {
    child.strip_suffix("  ").unwrap_or("").to_owned()
}
fn quote_json(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}
fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthScheme;
    use hsin_core::ModelSlot;

    type NamedItems = Vec<(String, String)>;

    fn target(client: ClientKind) -> ConfigTarget {
        ConfigTarget {
            client,
            mode: ConnectionMode::Direct,
            provider: Provider {
                id: "p".into(),
                client,
                name: "Provider".into(),
                description: String::new(),
                base_url: "https://example.test/v1".into(),
                auth_scheme: AuthScheme::Bearer,
                official: false,
                credential_configured: true,
                credential_preview: None,
                model: None,
                revision: 1,
                claude_model_mapping: None,
            },
            credential_command: "/opt/hsin".into(),
            proxy_host: "127.0.0.1".into(),
            proxy_port: 9999,
            disable_custom_auth: false,
            codex_auth_before_hash: None,
            claude_model_env_before: None,
        }
    }

    #[test]
    fn proxy_urls_support_ipv6_listener_hosts() {
        let mut codex = target(ClientKind::Codex);
        codex.mode = ConnectionMode::Proxy;
        codex.proxy_host = "::1".into();
        let codex_output = patch_codex("", &codex).unwrap();
        assert!(codex_output.contains("http://[::1]:9999/codex/v1"));

        let mut claude = target(ClientKind::Claude);
        claude.mode = ConnectionMode::Proxy;
        claude.proxy_host = "::1".into();
        let claude_output = patch_claude("", &claude).unwrap();
        assert!(claude_output.contains("http://[::1]:9999/claude"));
    }

    #[test]
    fn detects_official_and_custom_current_providers_without_exposing_secrets() {
        let official = detect_codex("model_provider = \"openai\"\n", "").unwrap();
        assert!(official.official);
        assert_eq!(official.auth_scheme, AuthScheme::OAuth);
        assert!(official.secret.is_none());

        let custom = detect_codex(
            "model_provider = \"acme\"\n[model_providers.acme]\nname = \"Acme\"\nbase_url = \"https://api.acme.test/v1\"\nexperimental_bearer_token = \"secret\"\n",
            "",
        )
        .unwrap();
        assert!(!custom.official);
        assert_eq!(custom.name, "Acme");
        assert_eq!(custom.auth_scheme, AuthScheme::Bearer);
        assert!(custom.secret.is_some());

        let login_backed = detect_codex(
            "model_provider = \"acme\"\n[model_providers.acme]\nname = \"Acme\"\nbase_url = \"https://api.acme.test/v1\"\nrequires_openai_auth = true\n",
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"auth-secret"}"#,
        )
        .unwrap();
        assert_eq!(
            login_backed.secret.as_deref().map(String::as_str),
            Some("auth-secret")
        );

        let claude = detect_claude(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://claude.acme.test","ANTHROPIC_API_KEY":"secret"}}"#,
        )
        .unwrap();
        assert!(!claude.official);
        assert_eq!(claude.auth_scheme, AuthScheme::XApiKey);
        assert!(claude.secret.is_some());

        let managed_codex = detect_codex(
            "model_provider = \"hsin\"\n[model_providers.hsin]\nbase_url = \"http://127.0.0.1:9999/codex/v1\"\nrequires_openai_auth = true\n",
            &format!(r#"{{"auth_mode":"apikey","OPENAI_API_KEY":"{HSIN_MANAGED_KEY}"}}"#),
        )
        .unwrap();
        assert!(managed_codex.secret.is_none());
        let managed_claude = detect_claude(&format!(
            r#"{{"env":{{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9999/claude","ANTHROPIC_API_KEY":"{HSIN_MANAGED_KEY}"}}}}"#
        ))
        .unwrap();
        assert!(managed_claude.secret.is_none());
    }

    #[test]
    fn claude_official_detection_rejects_custom_auth_fields() {
        let official = detect_claude("{}\n").unwrap();
        assert!(official.official);
        assert_eq!(official.auth_scheme, AuthScheme::OAuth);

        let api_key = detect_claude(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com","ANTHROPIC_API_KEY":"api-secret"}}"#,
        )
        .unwrap();
        assert!(!api_key.official);
        assert_eq!(api_key.auth_scheme, AuthScheme::XApiKey);
        assert_eq!(
            api_key.secret.as_deref().map(String::as_str),
            Some("api-secret")
        );

        let auth_token =
            detect_claude(r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"bearer-secret"}}"#).unwrap();
        assert!(!auth_token.official);
        assert_eq!(auth_token.auth_scheme, AuthScheme::Bearer);
        assert_eq!(
            auth_token.secret.as_deref().map(String::as_str),
            Some("bearer-secret")
        );

        let helper = detect_claude(r#"{"apiKeyHelper":"credential-helper"}"#).unwrap();
        assert!(!helper.official);
        assert_eq!(helper.auth_scheme, AuthScheme::XApiKey);
        assert!(helper.secret.is_none());

        let managed_key = detect_claude(&format!(
            r#"{{"env":{{"ANTHROPIC_API_KEY":"{HSIN_MANAGED_KEY}"}}}}"#
        ))
        .unwrap();
        assert!(!managed_key.official);
        assert!(managed_key.secret.is_none());
    }

    #[test]
    fn official_configuration_restores_native_auth_and_preserves_unowned_fields() {
        let mut codex = target(ClientKind::Codex);
        codex.provider = Provider {
            id: "official-codex".into(),
            client: ClientKind::Codex,
            name: "Official".into(),
            description: String::new(),
            base_url: CODEX_OFFICIAL_URL.into(),
            auth_scheme: AuthScheme::OAuth,
            official: true,
            credential_configured: false,
            credential_preview: None,
            model: None,
            revision: 1,
            claude_model_mapping: None,
        };
        let patched = patch_codex(
            "# keep\nmodel_provider = \"hsin\"\napproval_policy = \"never\"\n[model_providers.hsin]\nname = \"hsin\"\nbase_url = \"http://127.0.0.1:9999/codex/v1\"\n",
            &codex,
        )
        .unwrap();
        assert!(patched.contains("model_provider = \"hsin\""));
        assert!(patched.contains("requires_openai_auth = true"));
        assert!(patched.contains("# keep"));
        assert!(patched.contains("approval_policy = \"never\""));
        assert!(patched.contains("[model_providers.hsin]"));

        let mut claude = target(ClientKind::Claude);
        claude.provider = Provider {
            id: "official-claude".into(),
            client: ClientKind::Claude,
            name: "Official".into(),
            description: String::new(),
            base_url: CLAUDE_OFFICIAL_URL.into(),
            auth_scheme: AuthScheme::OAuth,
            official: true,
            credential_configured: false,
            credential_preview: None,
            model: None,
            revision: 1,
            claude_model_mapping: None,
        };
        let patched = patch_claude(
            r#"{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:9999/claude",
    "ANTHROPIC_API_KEY": "remove"
  },
  "apiKeyHelper": "remove",
  "hooks": {"keep": true}
}
"#,
            &claude,
        )
        .unwrap();
        assert!(!patched.contains("ANTHROPIC_BASE_URL"));
        assert!(!patched.contains("ANTHROPIC_API_KEY"));
        assert!(!patched.contains("apiKeyHelper"));
        assert!(patched.contains("\"hooks\""));
    }

    fn codex_unowned_items(text: &str) -> (NamedItems, NamedItems) {
        let document = text.parse::<DocumentMut>().unwrap();
        let root = document
            .iter()
            .filter(|(key, _)| *key != "model_provider" && *key != "model_providers")
            .map(|(key, item)| (key.to_owned(), item.to_string()))
            .collect();
        let providers = document
            .get("model_providers")
            .and_then(Item::as_table)
            .into_iter()
            .flat_map(Table::iter)
            .filter(|(key, _)| *key != "hsin")
            .map(|(key, item)| (key.to_owned(), item.to_string()))
            .collect();
        (root, providers)
    }

    fn claude_unowned_value(text: &str) -> serde_json::Value {
        let mut value =
            jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                .unwrap()
                .unwrap();
        let root = value.as_object_mut().unwrap();
        root.remove("apiKeyHelper");
        if let Some(env) = root
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
        {
            env.remove("ANTHROPIC_BASE_URL");
            env.remove("ANTHROPIC_API_KEY");
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
        value
    }

    fn assert_fragments_in_order(text: &str, fragments: &[&str]) {
        let mut offset = 0;
        for fragment in fragments {
            let relative = text[offset..]
                .find(fragment)
                .unwrap_or_else(|| panic!("missing byte-exact fragment: {fragment:?}\n{text}"));
            offset += relative + fragment.len();
        }
    }

    fn assert_crlf_only(text: &str) {
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                assert_eq!(
                    text.as_bytes().get(index.wrapping_sub(1)),
                    Some(&b'\r'),
                    "found a bare LF in CRLF configuration: {text:?}"
                );
            }
        }
    }

    #[test]
    fn codex_preserves_unmanaged_configuration() {
        let input = "# 心：必须原样保留\r\nmodel = \"gpt-test\" # 模型不可修改\r\nmodel_provider = \"old\" # provider 注释也保留\r\napproval_policy = \"on-request\"\r\n\r\n[features] # 功能顺序不可变\r\nweb_search = true\r\n\r\n[model_providers.keep] # 其他 provider 不可变\r\nname = \"狐娘\"\r\nbase_url = \"https://keep.example/原样\"\r\n\r\n[mcp_servers.keep]\r\ncommand = \"unchanged\"\r\n";
        let output = patch_codex(input, &target(ClientKind::Codex)).unwrap();

        assert_eq!(codex_unowned_items(input), codex_unowned_items(&output));
        assert_fragments_in_order(
            &output,
            &[
                "# 心：必须原样保留\r\nmodel = \"gpt-test\" # 模型不可修改\r\n",
                "model_provider = \"hsin\" # provider 注释也保留\r\n",
                "approval_policy = \"on-request\"\r\n\r\n[features] # 功能顺序不可变\r\nweb_search = true\r\n",
                "[model_providers.keep] # 其他 provider 不可变\r\nname = \"狐娘\"\r\nbase_url = \"https://keep.example/原样\"\r\n",
                "[mcp_servers.keep]\r\ncommand = \"unchanged\"\r\n",
            ],
        );
        assert_crlf_only(&output);
        assert!(output.contains("[model_providers.hsin.auth]"));
    }

    #[test]
    fn codex_patch_is_idempotent() {
        let input = "model = \"gpt-test\"\n[model_providers.keep]\nbase_url = \"https://keep\"\n";
        let once = patch_codex(input, &target(ClientKind::Codex)).unwrap();
        let twice = patch_codex(&once, &target(ClientKind::Codex)).unwrap();
        assert_eq!(twice, once);
    }

    #[test]
    fn codex_updates_model_only_when_provider_selects_one() {
        let input = "model = \"keep\" # preserve comment\napproval_policy = \"on-request\"\n";
        let mut selected = target(ClientKind::Codex);
        selected.provider.model = Some("gpt-5".into());
        let output = patch_codex(input, &selected).unwrap();
        assert!(output.contains("model = \"gpt-5\" # preserve comment"));
        assert!(output.contains("approval_policy = \"on-request\""));

        let unchanged = patch_codex(input, &target(ClientKind::Codex)).unwrap();
        assert!(unchanged.contains("model = \"keep\" # preserve comment"));
    }

    #[test]
    fn direct_helpers_bind_provider_revision_and_quote_executable() {
        let mut codex = target(ClientKind::Codex);
        codex.credential_command = "/Applications/HSIN App/hsin".into();
        let output = patch_codex("", &codex).unwrap();
        let document = output.parse::<DocumentMut>().unwrap();
        let args = document["model_providers"]["hsin"]["auth"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml_edit::Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "credential",
                "codex",
                "--provider-id",
                "p",
                "--revision",
                "1"
            ]
        );

        let mut claude = target(ClientKind::Claude);
        claude.credential_command = "/Applications/HSIN App/hsin".into();
        let output = patch_claude("{}\n", &claude).unwrap();
        let value =
            jsonc_parser::parse_to_serde_value(&output, &jsonc_parser::ParseOptions::default())
                .unwrap()
                .unwrap();
        let helper = value["apiKeyHelper"].as_str().unwrap();
        assert!(helper.contains("credential claude"));
        assert!(helper.contains("--provider-id"));
        assert!(helper.contains("--revision 1"));
        assert!(helper.contains("HSIN App"));

        claude.mode = ConnectionMode::Proxy;
        let proxy = patch_claude("{}\n", &claude).unwrap();
        assert!(!proxy.contains("--provider-id"));
        assert!(!proxy.contains("--revision"));
    }

    #[test]
    fn disabled_custom_auth_writes_direct_credentials_without_persisting_them_in_target() {
        let secret = "sk-direct-secret";
        let mut codex = target(ClientKind::Codex);
        codex.disable_custom_auth = true;
        let output = patch_codex_with_credential("", &codex, Some(secret)).unwrap();
        assert!(output.contains("requires_openai_auth = true"));
        assert!(!output.contains(secret));
        assert!(!output.contains("[model_providers.hsin.auth]"));
        assert!(!serde_json::to_string(&codex).unwrap().contains(secret));
        let auth = patch_codex_auth_text("{}\n", secret).unwrap();
        assert!(auth.contains(&format!("\"OPENAI_API_KEY\": \"{secret}\"")));
        assert!(auth.contains("\"auth_mode\": \"apikey\""));

        let mut claude = target(ClientKind::Claude);
        claude.provider.auth_scheme = AuthScheme::XApiKey;
        claude.disable_custom_auth = true;
        let output = patch_claude_with_credential("{}\n", &claude, Some(secret)).unwrap();
        assert!(output.contains(&format!("\"ANTHROPIC_API_KEY\": \"{secret}\"")));
        assert!(!output.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!output.contains("apiKeyHelper"));
    }

    #[test]
    fn disabled_custom_auth_uses_managed_key_in_proxy_mode() {
        let mut codex = target(ClientKind::Codex);
        codex.mode = ConnectionMode::Proxy;
        codex.disable_custom_auth = true;
        let output = patch_codex_with_credential("", &codex, None).unwrap();
        assert!(output.contains("requires_openai_auth = true"));
        assert!(!output.contains(HSIN_MANAGED_KEY));
        assert!(!output.contains("[model_providers.hsin.auth]"));
        let auth = patch_codex_auth_text("{}\n", HSIN_MANAGED_KEY).unwrap();
        assert!(auth.contains(&format!("\"OPENAI_API_KEY\": \"{HSIN_MANAGED_KEY}\"")));

        let mut claude = target(ClientKind::Claude);
        claude.mode = ConnectionMode::Proxy;
        claude.disable_custom_auth = true;
        let output = patch_claude_with_credential("{}\n", &claude, None).unwrap();
        assert!(output.contains(&format!("\"ANTHROPIC_API_KEY\": \"{HSIN_MANAGED_KEY}\"")));
        assert!(!output.contains("apiKeyHelper"));
    }

    #[test]
    fn codex_auth_uses_clean_json_layout_and_repairs_legacy_artifacts() {
        let expected = format!(
            "{{\n  \"auth_mode\": \"apikey\",\n  \"OPENAI_API_KEY\": \"{HSIN_MANAGED_KEY}\"\n}}\n"
        );
        assert_eq!(
            patch_codex_auth_text("{}\n", HSIN_MANAGED_KEY).unwrap(),
            expected
        );

        let malformed = "{\n\n\n  \"auth_mode\": \"apikey\"\n,\n  \"OPENAI_API_KEY\": \"stale\"}\n";
        assert_eq!(
            patch_codex_auth_text(malformed, HSIN_MANAGED_KEY).unwrap(),
            expected
        );

        let empty_snapshot = CodexAuthSnapshot {
            auth_path: "/tmp/auth.json".into(),
            file_existed: true,
            auth_mode: None,
            openai_api_key: None,
        };
        assert_eq!(
            restore_codex_auth_text(&expected, &empty_snapshot).unwrap(),
            "{\n}\n"
        );
    }

    #[test]
    fn claude_owned_fields_use_clean_json_layout_when_added_and_removed() {
        let mut claude = target(ClientKind::Claude);
        claude.mode = ConnectionMode::Proxy;
        claude.disable_custom_auth = true;
        let managed = patch_claude_with_credential("{}\n", &claude, None).unwrap();
        assert_eq!(
            managed,
            format!(
                "{{\n  \"env\": {{\n    \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:9999/claude\",\n    \"ANTHROPIC_API_KEY\": \"{HSIN_MANAGED_KEY}\"\n  }}\n}}\n"
            )
        );
        let malformed = "{\n  \"env\": {\n\n    \"ANTHROPIC_BASE_URL\": \"old\"\n,\n    \"ANTHROPIC_API_KEY\": \"old\"}\n}\n";
        assert_eq!(
            patch_claude_with_credential(malformed, &claude, None).unwrap(),
            managed
        );

        claude.provider = Provider {
            id: "official-claude".into(),
            client: ClientKind::Claude,
            name: "Official".into(),
            description: String::new(),
            base_url: CLAUDE_OFFICIAL_URL.into(),
            auth_scheme: AuthScheme::OAuth,
            official: true,
            credential_configured: false,
            credential_preview: None,
            model: None,
            revision: 1,
            claude_model_mapping: None,
        };
        assert_eq!(
            patch_claude_with_credential(&managed, &claude, None).unwrap(),
            "{\n  \"env\": {\n  }\n}\n"
        );
    }

    fn mapped(model: &str, context_1m: bool) -> ModelSlot {
        ModelSlot {
            model: model.into(),
            context_1m,
        }
    }

    #[test]
    fn claude_model_mapping_writes_each_mapped_tier_with_the_1m_suffix() {
        let mut claude = target(ClientKind::Claude);
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            default_model: Some("deepseek-v4-pro".into()),
            fable: Some(mapped("claude-fable-5", false)),
            opus: Some(mapped("claude-opus-5", true)),
            sonnet: None,
            haiku: Some(mapped("claude-haiku-4-5", false)),
        });
        let output = patch_claude_with_credential("", &claude, None).unwrap();
        assert!(output.contains("\"ANTHROPIC_DEFAULT_FABLE_MODEL\": \"claude-fable-5\""));
        assert!(output.contains("\"ANTHROPIC_DEFAULT_OPUS_MODEL\": \"claude-opus-5[1m]\""));
        assert!(output.contains("\"ANTHROPIC_DEFAULT_HAIKU_MODEL\": \"claude-haiku-4-5\""));
        // An unmapped tier is not written at all rather than being blanked out.
        assert!(!output.contains("\"ANTHROPIC_DEFAULT_SONNET_MODEL\""));

        // The session default outranks whatever selection Claude Code persisted, so it is the key
        // that keeps a stale first-party model ID from reaching an upstream that has never seen it.
        assert!(output.contains("\"ANTHROPIC_MODEL\": \"deepseek-v4-pro\""));
        // Each mapped tier also names itself in the picker; without these the picker lists the raw
        // ID and the mapping is invisible to the user.
        assert!(output.contains("\"ANTHROPIC_DEFAULT_OPUS_MODEL_NAME\": \"claude-opus-5\""));
        assert!(output.contains(
            "\"ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION\": \"Opus → claude-opus-5[1m]\""
        ));
        assert!(!output.contains("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"));
    }

    #[test]
    fn a_disabled_claude_model_mapping_restores_the_user_values_it_replaced() {
        let original = "{\n  \"env\": {\n    \"ANTHROPIC_DEFAULT_OPUS_MODEL\": \"mine\"\n  }\n}\n";
        let snapshot = ClaudeModelEnvSnapshot::capture(original).unwrap();
        assert_eq!(snapshot.get("ANTHROPIC_DEFAULT_OPUS_MODEL"), Some("mine"));

        let mut claude = target(ClientKind::Claude);
        claude.claude_model_env_before = Some(snapshot);
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let managed = patch_claude_with_credential(original, &claude, None).unwrap();
        assert!(managed.contains("\"ANTHROPIC_DEFAULT_OPUS_MODEL\": \"claude-opus-5\""));

        // Turning the mapping off puts the user's own value back instead of deleting the key.
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: false,
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let restored = patch_claude_with_credential(&managed, &claude, None).unwrap();
        assert!(restored.contains("\"ANTHROPIC_DEFAULT_OPUS_MODEL\": \"mine\""));
    }

    #[test]
    fn a_provider_without_a_mapping_removes_keys_the_user_never_set() {
        let mut claude = target(ClientKind::Claude);
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let managed = patch_claude_with_credential("", &claude, None).unwrap();
        assert!(managed.contains("ANTHROPIC_DEFAULT_OPUS_MODEL"));

        // Switching to a provider with no mapping at all, with an empty snapshot, clears the key.
        claude.provider.claude_model_mapping = None;
        let cleared = patch_claude_with_credential(&managed, &claude, None).unwrap();
        assert!(!cleared.contains("ANTHROPIC_DEFAULT_OPUS_MODEL"));
    }

    #[test]
    fn the_claude_model_mapping_owns_anthropic_model_without_losing_the_users_own() {
        let original = "{\n  \"env\": {\n    \"ANTHROPIC_MODEL\": \"user-choice\"\n  }\n}\n";
        let snapshot = ClaudeModelEnvSnapshot::capture(original).unwrap();
        let mut claude = target(ClientKind::Claude);
        claude.claude_model_env_before = Some(snapshot);

        // A mapping that names no default model leaves the user's own selection alone: hsin owns
        // the key, but owning it is not the same as always having a value for it.
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let untouched = patch_claude_with_credential(original, &claude, None).unwrap();
        assert!(untouched.contains("\"ANTHROPIC_MODEL\": \"user-choice\""));

        // Naming one replaces it, because a stale first-party selection persisted in settings.json
        // would otherwise outrank the tier mapping and be sent to a provider that never heard of it.
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            default_model: Some("deepseek-v4-pro".into()),
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let managed = patch_claude_with_credential(original, &claude, None).unwrap();
        assert!(managed.contains("\"ANTHROPIC_MODEL\": \"deepseek-v4-pro\""));

        // And dropping it hands the key back rather than deleting it.
        claude.provider.claude_model_mapping = None;
        let restored = patch_claude_with_credential(&managed, &claude, None).unwrap();
        assert!(restored.contains("\"ANTHROPIC_MODEL\": \"user-choice\""));
    }

    #[test]
    fn a_snapshot_from_an_older_hsin_still_captures_the_keys_added_since() {
        // Older snapshots record no key list. They must be read as covering only the four tier
        // keys that version owned, so the user's own `ANTHROPIC_MODEL` is captured before hsin
        // first writes over it rather than being silently lost.
        let stored: ClaudeModelEnvSnapshot =
            serde_json::from_str("{\"values\":{\"ANTHROPIC_DEFAULT_OPUS_MODEL\":\"mine\"}}")
                .unwrap();
        assert!(!stored.is_complete());

        let mut snapshot = stored;
        snapshot
            .extend_uncovered("{\"env\":{\"ANTHROPIC_MODEL\":\"user-choice\"}}")
            .unwrap();
        assert!(snapshot.is_complete());
        assert_eq!(snapshot.get("ANTHROPIC_MODEL"), Some("user-choice"));

        // The tier key was already covered, so hsin's own value in the live file cannot overwrite
        // what was captured for it.
        snapshot
            .extend_uncovered("{\"env\":{\"ANTHROPIC_DEFAULT_OPUS_MODEL\":\"hsin-wrote-this\"}}")
            .unwrap();
        assert_eq!(snapshot.get("ANTHROPIC_DEFAULT_OPUS_MODEL"), Some("mine"));
    }

    #[test]
    fn official_claude_providers_ignore_the_model_mapping() {
        let mut claude = target(ClientKind::Claude);
        claude.provider.official = true;
        claude.provider.auth_scheme = AuthScheme::OAuth;
        claude.provider.claude_model_mapping = Some(hsin_core::ClaudeModelMapping {
            enabled: true,
            opus: Some(mapped("claude-opus-5", false)),
            ..hsin_core::ClaudeModelMapping::default()
        });
        let output = patch_claude_with_credential("", &claude, None).unwrap();
        assert!(!output.contains("ANTHROPIC_DEFAULT_OPUS_MODEL"));
    }

    #[test]
    fn codex_auth_patch_and_restore_preserve_unowned_fields() {
        let original = "{\r\n  // keep\r\n  \"auth_mode\": \"chatgpt\",\r\n  \"OPENAI_API_KEY\": \"old-secret\",\r\n  \"tokens\": { \"access_token\": \"keep-token\" },\r\n  \"account_id\": \"keep-account\"\r\n}\r\n";
        let snapshot = CodexAuthSnapshot {
            auth_path: "/tmp/auth.json".into(),
            file_existed: true,
            auth_mode: Some("chatgpt".into()),
            openai_api_key: Some("old-secret".into()),
        };
        let managed = patch_codex_auth_text(original, "new-secret").unwrap();
        assert!(managed.contains("\"auth_mode\": \"apikey\""));
        assert!(managed.contains("\"OPENAI_API_KEY\": \"new-secret\""));
        assert!(managed.contains("// keep\r\n"));
        assert!(managed.contains("\"tokens\": { \"access_token\": \"keep-token\" }"));
        assert!(managed.contains("\"account_id\": \"keep-account\""));
        assert_crlf_only(&managed);

        let restored = restore_codex_auth_text(&managed, &snapshot).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn codex_replaces_only_existing_hsin_subtree() {
        let input = "model = \"gpt-test\"\nmodel_provider = \"legacy\" # selector comment\n\n[model_providers.hsin]\nname = \"stale\"\nbase_url = \"https://stale.example\"\nlegacy_field = \"owned and removable\"\n\n[model_providers.hsin.auth]\ncommand = \"stale-helper\"\n\n# keep-table comment\n[model_providers.keep]\nbase_url = \"https://keep.example\"\n\n[profiles.\"心\"]\nmodel = \"不可修改\"\n";
        let output = patch_codex(input, &target(ClientKind::Codex)).unwrap();

        assert_eq!(codex_unowned_items(input), codex_unowned_items(&output));
        assert_fragments_in_order(
            &output,
            &[
                "model = \"gpt-test\"\nmodel_provider = \"hsin\" # selector comment\n",
                "[model_providers.hsin]\nname = \"hsin\"\nbase_url = \"https://example.test/v1\"\nwire_api = \"responses\"\n",
                "[model_providers.hsin.auth]\ncommand = \"/opt/hsin\"\n",
                "# keep-table comment\n[model_providers.keep]\nbase_url = \"https://keep.example\"\n\n[profiles.\"心\"]\nmodel = \"不可修改\"\n",
            ],
        );
        assert!(!output.contains("legacy_field"));
        assert!(!output.contains("stale-helper"));
    }

    #[test]
    fn claude_preserves_comments_and_unmanaged_fields() {
        let input = "{\n  // keep me\n  \"permissions\": { \"allow\": [\"Read(心)\"] },\n  \"env\": {\n    \"OTHER\": \"keep\", // inline comment\n    \"ANTHROPIC_SMALL_FAST_MODEL\": \"claude-test\",\n    \"ANTHROPIC_API_KEY\": \"remove\",\n    \"ANTHROPIC_AUTH_TOKEN\": \"remove too\"\n  },\n  \"hooks\": {\"Stop\": []},\n  \"mcpServers\": { \"keep\": { \"command\": \"unchanged\" } }\n}\n";
        let output = patch_claude(input, &target(ClientKind::Claude)).unwrap();

        assert_eq!(claude_unowned_value(input), claude_unowned_value(&output));
        assert_fragments_in_order(
            &output,
            &[
                "// keep me\n  \"permissions\": { \"allow\": [\"Read(心)\"] },",
                "\"OTHER\": \"keep\", // inline comment\n    \"ANTHROPIC_SMALL_FAST_MODEL\": \"claude-test\"",
                "\"hooks\": {\"Stop\": []},\n  \"mcpServers\": { \"keep\": { \"command\": \"unchanged\" } }",
            ],
        );
        assert!(!output.contains("ANTHROPIC_API_KEY"));
        assert!(!output.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(output.contains("apiKeyHelper"));
    }

    #[test]
    fn claude_accepts_trailing_commas_crlf_and_unicode() {
        let input = "{\r\n  // 保留\r\n  \"env\": {\r\n    \"OTHER\": \"狐娘\",\r\n  },\r\n  \"hooks\": {\"Stop\": [],},\r\n}\r\n";
        let output = patch_claude(input, &target(ClientKind::Claude)).unwrap();
        assert_eq!(claude_unowned_value(input), claude_unowned_value(&output));
        assert_fragments_in_order(
            &output,
            &[
                "// 保留\r\n",
                "\"OTHER\": \"狐娘\",\r\n",
                "\"hooks\": {\"Stop\": [],},\r\n",
            ],
        );
        assert_crlf_only(&output);
        validate_jsonc(&output).unwrap();
    }

    #[test]
    fn claude_patch_is_idempotent() {
        let input = "{\n  \"env\": {\n    \"OTHER\": \"keep\"\n  },\n  \"hooks\": {}\n}\n";
        let once = patch_claude(input, &target(ClientKind::Claude)).unwrap();
        let twice = patch_claude(&once, &target(ClientKind::Claude)).unwrap();
        assert_eq!(twice, once);
    }

    #[test]
    fn atomic_patch_rejects_external_changes() {
        let root = std::env::temp_dir().join(format!("hsin-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(&path, "model = \"one\"\n").unwrap();
        let expected = file_hash(&path).unwrap();
        fs::write(&path, "model = \"two\"\n").unwrap();
        let result = apply(&path, expected.as_deref(), &target(ClientKind::Codex));
        assert!(matches!(result, Err(DaemonError::Conflict(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), "model = \"two\"\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_patch_leaves_original_when_update_fails() {
        let root =
            std::env::temp_dir().join(format!("hsin-failed-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        let original = b"{\n  \"hooks\": {}\n}\n";
        fs::write(&path, original).unwrap();
        let expected = file_hash(&path).unwrap();

        let result = atomic_patch(&path, expected.as_deref(), |_| {
            Err(DaemonError::Config("injected patch failure".into()))
        });

        assert!(matches!(result, Err(DaemonError::Config(_))));
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_apply_is_a_noop_with_current_hash() {
        let root = std::env::temp_dir().join(format!("hsin-repeat-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(&path, "model = \"keep\"\n").unwrap();

        let first_hash = file_hash(&path).unwrap();
        let applied_hash = apply(&path, first_hash.as_deref(), &target(ClientKind::Codex)).unwrap();
        let once = fs::read(&path).unwrap();
        let repeated_hash = apply(&path, Some(&applied_hash), &target(ClientKind::Codex)).unwrap();

        assert_eq!(repeated_hash, applied_hash);
        assert_eq!(fs::read(&path).unwrap(), once);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_patch_leaves_original_when_temporary_file_cannot_be_created() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("hsin-atomic-open-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        let lock_path = path.with_extension("toml.hsin.lock");
        let original = b"model = \"keep\"\n";
        fs::write(&path, original).unwrap();
        fs::write(&lock_path, b"").unwrap();
        let expected = file_hash(&path).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();

        let result = atomic_patch(&path, expected.as_deref(), |_| {
            Ok("model = \"changed\"\n".into())
        });

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(DaemonError::Io(_))));
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_patch_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("hsin-mode-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(&path, "model = \"keep\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let expected = file_hash(&path).unwrap();
        apply(&path, expected.as_deref(), &target(ClientKind::Codex)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(root).unwrap();
    }
}
