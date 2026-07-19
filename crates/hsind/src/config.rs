use std::{
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
use zeroize::Zeroizing;

use crate::{
    error::{DaemonError, Result},
    model::{AuthScheme, ClientKind, ConnectionMode, Provider},
};

pub const CODEX_OFFICIAL_URL: &str = "https://api.openai.com/v1";
pub const CLAUDE_OFFICIAL_URL: &str = "https://api.anthropic.com";

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
    pub proxy_port: u16,
}

pub fn default_config_path(client: ClientKind) -> Result<PathBuf> {
    let home = BaseDirs::new()
        .ok_or_else(|| DaemonError::Config("cannot resolve the user home directory".into()))?
        .home_dir()
        .to_path_buf();
    Ok(match client {
        ClientKind::Codex => std::env::var_os("CODEX_HOME")
            .map_or_else(|| home.join(".codex"), PathBuf::from)
            .join("config.toml"),
        ClientKind::Claude => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map_or_else(|| home.join(".claude"), PathBuf::from)
            .join("settings.json"),
    })
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
        ClientKind::Codex => detect_codex(&text),
        ClientKind::Claude => detect_claude(&text),
    }
}

fn detect_codex(text: &str) -> Result<DetectedProvider> {
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
    let secret = provider
        .get("experimental_bearer_token")
        .and_then(Item::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| Zeroizing::new(value.to_owned()))
        .or_else(|| {
            provider
                .get("env_key")
                .and_then(Item::as_str)
                .and_then(|key| std::env::var(key).ok())
                .filter(|value| !value.trim().is_empty())
                .map(Zeroizing::new)
        });
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
    if same_url(&base_url, CLAUDE_OFFICIAL_URL) {
        return Ok(official_provider(ClientKind::Claude));
    }
    let auth_token = env
        .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let api_key = env
        .and_then(|env| env.get("ANTHROPIC_API_KEY"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    Ok(DetectedProvider {
        name: imported_name("Claude", &base_url),
        base_url,
        auth_scheme: if auth_token.is_some() {
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

pub fn apply(path: &Path, expected_hash: Option<&str>, target: &ConfigTarget) -> Result<String> {
    atomic_patch(path, expected_hash, |before| patch_text(before, target))
}

pub fn patch_text(before: &str, target: &ConfigTarget) -> Result<String> {
    match target.client {
        ClientKind::Codex => patch_codex(before, target),
        ClientKind::Claude => patch_claude(before, target),
    }
}

pub fn file_hash(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(hash(&fs::read(path)?)))
}

pub fn patch_codex(text: &str, target: &ConfigTarget) -> Result<String> {
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
    let provider_block = codex_provider_block(target, newline(&output));
    match document.get("model_providers") {
        Some(Item::Table(providers)) => match providers.get("hsin") {
            Some(Item::Value(existing)) => {
                let span = existing.span().ok_or_else(|| {
                    DaemonError::Config("hsin provider has no source span".into())
                })?;
                output.replace_range(span, &codex_provider_inline(target)?);
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

fn codex_provider_table(target: &ConfigTarget) -> Table {
    let base_url = match target.mode {
        ConnectionMode::Direct => target.provider.base_url.trim_end_matches('/').to_owned(),
        ConnectionMode::Proxy => format!("http://127.0.0.1:{}/codex/v1", target.proxy_port),
    };
    let mut provider = Table::new();
    provider.set_implicit(false);
    provider["name"] = value("hsin");
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    if target.provider.official {
        provider["requires_openai_auth"] = value(true);
        return provider;
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
    provider
}

fn codex_provider_block(target: &ConfigTarget, newline: &str) -> String {
    let mut document = DocumentMut::new();
    let mut providers = Table::new();
    providers.set_implicit(true);
    providers.insert("hsin", Item::Table(codex_provider_table(target)));
    document["model_providers"] = Item::Table(providers);
    document.to_string().replace('\n', newline)
}

fn codex_provider_inline(target: &ConfigTarget) -> Result<String> {
    Item::Table(codex_provider_table(target))
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

pub fn patch_claude(text: &str, target: &ConfigTarget) -> Result<String> {
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
        ConnectionMode::Proxy => format!("http://127.0.0.1:{}/claude", target.proxy_port),
    };
    output = set_nested_string(&output, "env", "ANTHROPIC_BASE_URL", Some(&base_url))?;
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
            return Ok(format!(
                "{}{}{}",
                &text[..existing.value_start],
                raw,
                &text[existing.value_end..]
            ));
        }
        return remove_property(text, range, existing);
    }
    let Some(raw) = raw else {
        return Ok(text.to_owned());
    };
    let indent = child_indent(text, range);
    let has_properties = skip_trivia(text, range.start + 1)? < range.end;
    let newline = newline(text);
    let insertion = if has_properties {
        let separator = if object_has_trailing_comma(text, range)? {
            ""
        } else {
            ","
        };
        format!(
            "{separator}{newline}{indent}{}: {raw}",
            quote_json(property)
        )
    } else {
        format!(
            "{newline}{indent}{}: {raw}{newline}{}",
            quote_json(property),
            parent_indent(&indent)
        )
    };
    Ok(format!(
        "{}{}{}",
        &text[..range.end],
        insertion,
        &text[range.end..]
    ))
}

fn remove_property(text: &str, range: ObjectRange, property: PropertyRange) -> Result<String> {
    let bytes = text.as_bytes();
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

fn object_has_trailing_comma(text: &str, range: ObjectRange) -> Result<bool> {
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
        return Ok(false);
    };
    Ok(text.as_bytes().get(skip_trivia(text, value_end)?) == Some(&b','))
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
            },
            credential_command: "/opt/hsin".into(),
            proxy_port: 9999,
        }
    }

    #[test]
    fn detects_official_and_custom_current_providers_without_exposing_secrets() {
        let official = detect_codex("model_provider = \"openai\"\n").unwrap();
        assert!(official.official);
        assert_eq!(official.auth_scheme, AuthScheme::OAuth);
        assert!(official.secret.is_none());

        let custom = detect_codex(
            "model_provider = \"acme\"\n[model_providers.acme]\nname = \"Acme\"\nbase_url = \"https://api.acme.test/v1\"\nexperimental_bearer_token = \"secret\"\n",
        )
        .unwrap();
        assert!(!custom.official);
        assert_eq!(custom.name, "Acme");
        assert_eq!(custom.auth_scheme, AuthScheme::Bearer);
        assert!(custom.secret.is_some());

        let claude = detect_claude(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://claude.acme.test","ANTHROPIC_API_KEY":"secret"}}"#,
        )
        .unwrap();
        assert!(!claude.official);
        assert_eq!(claude.auth_scheme, AuthScheme::XApiKey);
        assert!(claude.secret.is_some());
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
        let input = "{\n  // keep me\n  \"permissions\": { \"allow\": [\"Read(心)\"] },\n  \"env\": {\n    \"OTHER\": \"keep\", // inline comment\n    \"ANTHROPIC_MODEL\": \"claude-test\",\n    \"ANTHROPIC_API_KEY\": \"remove\",\n    \"ANTHROPIC_AUTH_TOKEN\": \"remove too\"\n  },\n  \"hooks\": {\"Stop\": []},\n  \"mcpServers\": { \"keep\": { \"command\": \"unchanged\" } }\n}\n";
        let output = patch_claude(input, &target(ClientKind::Claude)).unwrap();

        assert_eq!(claude_unowned_value(input), claude_unowned_value(&output));
        assert_fragments_in_order(
            &output,
            &[
                "// keep me\n  \"permissions\": { \"allow\": [\"Read(心)\"] },",
                "\"OTHER\": \"keep\", // inline comment\n    \"ANTHROPIC_MODEL\": \"claude-test\"",
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
