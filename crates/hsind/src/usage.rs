use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local, Utc};
use hsin_core::{
    ClientKind, ConnectionMode, UsageAttribution, UsageAttributionCounts, UsageDailyBucket,
    UsageDataSource, UsageFilterOptions, UsageModelBreakdown, UsageProviderBreakdown,
    UsageProviderOption, UsageStatsQuery, UsageStatsReport, UsageSyncResult, UsageTokenSummary,
};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{db::Database, error::Result};

const STARTED_AT_KEY: &str = "usage_started_at";
const LAST_SYNCED_AT_KEY: &str = "usage_last_synced_at";
const SYNC_STATUS_KEY: &str = "usage_sync_status";
const LAST_CLEANUP_AT_KEY: &str = "usage_last_cleanup_at";
const MAX_SESSION_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PROXY_OBSERVATION_BYTES: usize = 1024 * 1024;
const RETENTION_DAYS: i64 = 90;

#[derive(Debug, Clone)]
pub(crate) struct UsageEvent {
    pub client: ClientKind,
    pub source: UsageDataSource,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub provider_revision: u64,
    pub model: String,
    pub event_at: i64,
    pub tokens: UsageTokenSummary,
    pub attribution: UsageAttribution,
    pub dedup_key: String,
    pub correlation_key: Option<String>,
}

#[derive(Debug, Clone)]
struct Cursor {
    modified_at: i64,
    file_size: u64,
    byte_offset: u64,
    tail_fingerprint: String,
}

#[derive(Debug, Clone)]
struct StoredEvent {
    provider_id: Option<String>,
    provider_name: String,
    provider_revision: u64,
    model: String,
    event_at: i64,
    tokens: UsageTokenSummary,
    attribution: UsageAttribution,
}

type UsageRoute = (Option<String>, String, u64, ConnectionMode);

pub(crate) struct UsageCollector {
    db: Arc<Database>,
    codex_home: PathBuf,
    claude_home: PathBuf,
    sync_lock: Mutex<()>,
}

pub(crate) struct ProxyUsageObserver {
    collector: Arc<UsageCollector>,
    provider_id: String,
    provider_name: String,
    provider_revision: u64,
    client: ClientKind,
    event_at: i64,
    is_sse: bool,
    buffer: Vec<u8>,
    exceeded: bool,
    recorded: bool,
    claude_id: Option<String>,
    claude_model: Option<String>,
    fallback_model: Option<String>,
    claude_tokens: UsageTokenSummary,
}

impl UsageCollector {
    pub(crate) fn new(db: Arc<Database>, codex_config: &Path, claude_config: &Path) -> Arc<Self> {
        Arc::new(Self {
            db,
            codex_home: codex_config
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            claude_home: claude_config
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            sync_lock: Mutex::new(()),
        })
    }

    pub(crate) fn initialize(&self) -> Result<()> {
        let _guard = self.sync_lock.lock();
        let now = unix_time();
        let first_start = self.meta(STARTED_AT_KEY)?.is_none();
        if first_start {
            let existing = self
                .session_files()
                .into_iter()
                .filter_map(|(client, path)| {
                    let metadata = fs::metadata(&path).ok()?;
                    let offset = last_complete_offset(&path, metadata.len()).ok()?;
                    let fingerprint = tail_fingerprint(&path, offset).ok()?;
                    Some((client, path, metadata, offset, fingerprint))
                })
                .collect::<Vec<_>>();
            self.set_meta(STARTED_AT_KEY, &now.to_string(), now)?;
            for client in ClientKind::ALL {
                self.record_route(client, now)?;
            }
            for (client, path, metadata, offset, fingerprint) in existing {
                self.save_cursor(
                    &path_hash(&path),
                    client,
                    &Cursor {
                        modified_at: modified_seconds(&metadata),
                        file_size: metadata.len(),
                        byte_offset: offset,
                        tail_fingerprint: fingerprint,
                    },
                    now,
                )?;
            }
        } else {
            for client in ClientKind::ALL {
                self.record_route(client, now)?;
            }
        }
        self.cleanup_if_due(now, true)?;
        Ok(())
    }

    pub(crate) fn record_current_route(&self, client: ClientKind) -> Result<()> {
        self.record_route(client, unix_time())
    }

    pub(crate) fn proxy_observer(
        self: &Arc<Self>,
        client: ClientKind,
        provider_id: String,
        provider_name: String,
        provider_revision: u64,
        fallback_model: Option<String>,
        is_sse: bool,
    ) -> ProxyUsageObserver {
        ProxyUsageObserver {
            collector: self.clone(),
            provider_id,
            provider_name,
            provider_revision,
            client,
            event_at: unix_time(),
            is_sse,
            buffer: Vec::new(),
            exceeded: false,
            recorded: false,
            claude_id: None,
            claude_model: None,
            fallback_model,
            claude_tokens: UsageTokenSummary::default(),
        }
    }

    fn record_route(&self, client: ClientKind, at: i64) -> Result<()> {
        let state = self.db.client_state(client)?;
        let provider = state
            .active_provider_id
            .as_deref()
            .and_then(|id| self.db.get_provider(id).ok());
        let connection = self.db.connection.lock();
        connection.execute(
            "INSERT INTO usage_routes(client,effective_at,provider_id,provider_name,provider_revision,mode) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(client,effective_at) DO UPDATE SET provider_id=excluded.provider_id,provider_name=excluded.provider_name,provider_revision=excluded.provider_revision,mode=excluded.mode",
            params![
                client.to_string(),
                at,
                provider.as_ref().map(|value| value.id.as_str()),
                provider.as_ref().map_or("Unattributed", |value| value.name.as_str()),
                provider.as_ref().map_or(0, |value| value.revision),
                state.mode.to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<UsageSyncResult> {
        let _guard = self.sync_lock.lock();
        let now = unix_time();
        let started_at = self
            .meta(STARTED_AT_KEY)?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(now);
        let mut result = UsageSyncResult {
            synced_at: now,
            ..UsageSyncResult::default()
        };
        for (client, path) in self.session_files() {
            if self
                .sync_file(client, &path, started_at, &mut result)
                .is_err()
            {
                result.failed_files = result.failed_files.saturating_add(1);
            }
        }
        self.set_meta(LAST_SYNCED_AT_KEY, &now.to_string(), now)?;
        self.set_meta(
            SYNC_STATUS_KEY,
            if result.failed_files == 0 {
                "ok"
            } else {
                "partial"
            },
            now,
        )?;
        self.cleanup_if_due(now, false)?;
        Ok(result)
    }

    fn session_files(&self) -> Vec<(ClientKind, PathBuf)> {
        let mut files = Vec::new();
        for root in [
            self.codex_home.join("sessions"),
            self.codex_home.join("archived_sessions"),
        ] {
            collect_jsonl(&root, ClientKind::Codex, &mut files);
        }
        collect_jsonl(&self.claude_home, ClientKind::Claude, &mut files);
        files.sort_by(|left, right| left.1.cmp(&right.1));
        files
    }

    fn sync_file(
        &self,
        client: ClientKind,
        path: &Path,
        started_at: i64,
        result: &mut UsageSyncResult,
    ) -> Result<()> {
        let metadata = fs::metadata(path)?;
        let hash = path_hash(path);
        let mut cursor = self.cursor(&hash)?.unwrap_or(Cursor {
            modified_at: 0,
            file_size: 0,
            byte_offset: 0,
            tail_fingerprint: String::new(),
        });
        if metadata.len() < cursor.byte_offset
            || (cursor.byte_offset > 0
                && tail_fingerprint(path, cursor.byte_offset)? != cursor.tail_fingerprint)
        {
            cursor.byte_offset = 0;
        }
        if metadata.len() == cursor.byte_offset && modified_seconds(&metadata) == cursor.modified_at
        {
            return Ok(());
        }

        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(cursor.byte_offset))?;
        let mut offset = cursor.byte_offset;
        let mut codex_model = String::from("unknown");
        let mut codex_cumulative: Option<UsageTokenSummary> = None;
        loop {
            let line_start = offset;
            let mut bytes = Vec::new();
            let read = reader.read_until(b'\n', &mut bytes)?;
            if read == 0 {
                break;
            }
            if !bytes.ends_with(b"\n") {
                break;
            }
            offset = offset.saturating_add(read as u64);
            if bytes.len() > MAX_SESSION_LINE_BYTES {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            }
            while matches!(bytes.last(), Some(b'\n' | b'\r')) {
                bytes.pop();
            }
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            };
            let parsed = match client {
                ClientKind::Claude => parse_claude_session(&value),
                ClientKind::Codex => {
                    parse_codex_session(&value, &mut codex_model, &mut codex_cumulative)
                }
            };
            let Some(mut parsed) = parsed else {
                continue;
            };
            if parsed.event_at < started_at || parsed.event_at < retention_cutoff() {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            }
            let route = self.route_at(client, parsed.event_at)?;
            if let Some((provider_id, provider_name, provider_revision, _mode)) = route {
                parsed.provider_id = provider_id;
                parsed.provider_name = provider_name;
                parsed.provider_revision = provider_revision;
                parsed.attribution = if parsed.provider_id.is_some() {
                    UsageAttribution::Inferred
                } else {
                    UsageAttribution::Unattributed
                };
            }
            if parsed.dedup_key.is_empty() {
                parsed.dedup_key = parsed.correlation_key.as_ref().map_or_else(
                    || digest(&format!("session:{client}:{hash}:{line_start}")),
                    |correlation| {
                        digest(&format!(
                            "session:{client}:{correlation}:{}",
                            parsed.event_at
                        ))
                    },
                );
            }
            if self.insert_event(&parsed)? {
                result.imported = result.imported.saturating_add(1);
            } else {
                result.skipped = result.skipped.saturating_add(1);
            }
        }
        self.save_cursor(
            &hash,
            client,
            &Cursor {
                modified_at: modified_seconds(&metadata),
                file_size: metadata.len(),
                byte_offset: offset,
                tail_fingerprint: tail_fingerprint(path, offset)?,
            },
            unix_time(),
        )
    }

    pub(crate) fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let connection = self.db.connection.lock();
        if event.source == UsageDataSource::Proxy
            && let Some(correlation) = &event.correlation_key
        {
            connection.execute(
                "DELETE FROM usage_events WHERE id=(SELECT id FROM usage_events WHERE client=?1 AND source='session' AND correlation_key=?2 AND abs(event_at-?3)<=120 ORDER BY abs(event_at-?3) LIMIT 1)",
                params![event.client.to_string(), correlation, event.event_at],
            )?;
        }
        let changed = connection.execute(
            "INSERT INTO usage_events(client,source,provider_id,provider_name,provider_revision,model,event_at,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,reasoning_output_tokens,attribution,dedup_key,correlation_key,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) ON CONFLICT(dedup_key) DO UPDATE SET source=CASE WHEN excluded.source='proxy' THEN excluded.source ELSE source END,provider_id=CASE WHEN excluded.source='proxy' THEN excluded.provider_id ELSE provider_id END,provider_name=CASE WHEN excluded.source='proxy' THEN excluded.provider_name ELSE provider_name END,provider_revision=CASE WHEN excluded.source='proxy' THEN excluded.provider_revision ELSE provider_revision END,attribution=CASE WHEN excluded.source='proxy' THEN excluded.attribution ELSE attribution END,output_tokens=max(output_tokens,excluded.output_tokens),reasoning_output_tokens=max(reasoning_output_tokens,excluded.reasoning_output_tokens)",
            params![
                event.client.to_string(), event.source.as_str(), event.provider_id,
                event.provider_name, event.provider_revision, event.model, event.event_at,
                event.tokens.input_tokens, event.tokens.cache_write_tokens,
                event.tokens.cache_read_tokens, event.tokens.output_tokens,
                event.tokens.reasoning_output_tokens, event.attribution.as_str(),
                event.dedup_key, event.correlation_key, unix_time(),
            ],
        )?;
        Ok(changed > 0)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn query(&self, query: UsageStatsQuery) -> Result<UsageStatsReport> {
        if query.from >= query.to {
            return Err(crate::error::DaemonError::Invalid(
                "usage query start must be earlier than its end".into(),
            ));
        }
        let connection = self.db.connection.lock();
        let mut statement = connection.prepare(
            "SELECT provider_id,provider_name,provider_revision,model,event_at,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,reasoning_output_tokens,attribution FROM usage_events WHERE client=?1 AND event_at>=?2 AND event_at<?3 ORDER BY event_at",
        )?;
        let rows = statement.query_map(
            params![query.client.to_string(), query.from, query.to],
            stored_event_from_row,
        )?;
        let all = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        drop(connection);

        let filters = filter_options(&all);
        let events = all.iter().filter(|event| {
            query
                .provider_id
                .as_ref()
                .is_none_or(|id| event.provider_id.as_ref() == Some(id))
                && query
                    .model
                    .as_ref()
                    .is_none_or(|model| &event.model == model)
        });
        let mut summary = UsageTokenSummary::default();
        let mut daily = BTreeMap::<String, UsageTokenSummary>::new();
        let mut providers =
            HashMap::<(Option<String>, String, u64), (bool, UsageTokenSummary)>::new();
        let mut models =
            HashMap::<String, (UsageTokenSummary, BTreeMap<String, UsageTokenSummary>)>::new();
        let mut attribution = UsageAttributionCounts::default();
        for event in events {
            add_tokens(&mut summary, &event.tokens);
            let date = local_date(event.event_at);
            add_tokens(daily.entry(date.clone()).or_default(), &event.tokens);
            let provider = providers
                .entry((
                    event.provider_id.clone(),
                    event.provider_name.clone(),
                    event.provider_revision,
                ))
                .or_default();
            provider.0 |= event.attribution == UsageAttribution::Inferred;
            add_tokens(&mut provider.1, &event.tokens);
            let model = models.entry(event.model.clone()).or_default();
            add_tokens(&mut model.0, &event.tokens);
            add_tokens(model.1.entry(date).or_default(), &event.tokens);
            match event.attribution {
                UsageAttribution::Exact => attribution.exact += 1,
                UsageAttribution::Inferred => attribution.inferred += 1,
                UsageAttribution::Unattributed => attribution.unattributed += 1,
            }
        }
        let mut provider_rows = providers
            .into_iter()
            .map(
                |((provider_id, provider_name, provider_revision), (inferred, tokens))| {
                    UsageProviderBreakdown {
                        provider_id,
                        provider_name,
                        provider_revision,
                        inferred,
                        tokens,
                    }
                },
            )
            .collect::<Vec<_>>();
        provider_rows.sort_by_key(|row| std::cmp::Reverse(row.tokens.total_tokens()));
        let dates = date_range(query.from.max(retention_cutoff()), query.to);
        let mut model_rows = models
            .into_iter()
            .map(|(model, (tokens, daily))| UsageModelBreakdown {
                model,
                tokens,
                daily: dates
                    .iter()
                    .map(|date| UsageDailyBucket {
                        date: date.clone(),
                        tokens: daily.get(date).cloned().unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        model_rows.sort_by_key(|row| std::cmp::Reverse(row.tokens.total_tokens()));
        let collected_since = self
            .meta(STARTED_AT_KEY)?
            .and_then(|value| value.parse().ok())
            .unwrap_or(query.from);
        let last_synced_at = self
            .meta(LAST_SYNCED_AT_KEY)?
            .and_then(|value| value.parse().ok());
        let total_tokens = summary.total_tokens();
        let hit_tokens = summary.cache_read_tokens;
        let non_hit_tokens = summary.non_hit_tokens();
        let cache_hit_rate = summary.cache_hit_rate();
        Ok(UsageStatsReport {
            query,
            summary,
            total_tokens,
            hit_tokens,
            non_hit_tokens,
            cache_hit_rate,
            daily: dates
                .into_iter()
                .map(|date| UsageDailyBucket {
                    tokens: daily.get(&date).cloned().unwrap_or_default(),
                    date,
                })
                .collect(),
            providers: provider_rows,
            models: model_rows,
            attribution,
            filters,
            collected_since,
            last_synced_at,
        })
    }

    fn cursor(&self, hash: &str) -> Result<Option<Cursor>> {
        self.db
            .connection
            .lock()
            .query_row(
                "SELECT modified_at,file_size,byte_offset,tail_fingerprint FROM usage_sync_cursors WHERE path_hash=?1",
                [hash],
                |row| Ok(Cursor { modified_at: row.get(0)?, file_size: row.get(1)?, byte_offset: row.get(2)?, tail_fingerprint: row.get(3)? }),
            )
            .optional()
            .map_err(Into::into)
    }

    fn save_cursor(&self, hash: &str, client: ClientKind, cursor: &Cursor, now: i64) -> Result<()> {
        self.db.connection.lock().execute(
            "INSERT INTO usage_sync_cursors(path_hash,client,modified_at,file_size,byte_offset,tail_fingerprint,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(path_hash) DO UPDATE SET client=excluded.client,modified_at=excluded.modified_at,file_size=excluded.file_size,byte_offset=excluded.byte_offset,tail_fingerprint=excluded.tail_fingerprint,updated_at=excluded.updated_at",
            params![hash, client.to_string(), cursor.modified_at, cursor.file_size, cursor.byte_offset, cursor.tail_fingerprint, now],
        )?;
        Ok(())
    }

    fn route_at(&self, client: ClientKind, at: i64) -> Result<Option<UsageRoute>> {
        self.db.connection.lock().query_row(
            "SELECT provider_id,provider_name,provider_revision,mode FROM usage_routes WHERE client=?1 AND effective_at<=?2 ORDER BY effective_at DESC LIMIT 1",
            params![client.to_string(), at],
            |row| {
                let mode: String = row.get(3)?;
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, ConnectionMode::from_str(&mode).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?))
            },
        ).optional().map_err(Into::into)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        self.db
            .connection
            .lock()
            .query_row("SELECT value FROM usage_meta WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    fn set_meta(&self, key: &str, value: &str, now: i64) -> Result<()> {
        self.db.connection.lock().execute(
            "INSERT INTO usage_meta(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    fn cleanup_if_due(&self, now: i64, force: bool) -> Result<()> {
        let last = self
            .meta(LAST_CLEANUP_AT_KEY)?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        if force || now.saturating_sub(last) >= 24 * 60 * 60 {
            self.db.connection.lock().execute(
                "DELETE FROM usage_events WHERE event_at<?1",
                [retention_cutoff()],
            )?;
            self.set_meta(LAST_CLEANUP_AT_KEY, &now.to_string(), now)?;
        }
        Ok(())
    }
}

impl ProxyUsageObserver {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.exceeded || self.recorded {
            return;
        }
        if self.buffer.len().saturating_add(bytes.len()) > MAX_PROXY_OBSERVATION_BYTES {
            self.buffer.clear();
            self.exceeded = true;
            return;
        }
        self.buffer.extend_from_slice(bytes);
        if self.is_sse {
            self.consume_sse_events();
        }
    }

    pub(crate) fn finish(mut self) {
        if self.exceeded || self.recorded {
            return;
        }
        if self.is_sse {
            self.consume_sse_events();
            if self.client == ClientKind::Claude && self.claude_tokens.total_tokens() > 0 {
                self.record_claude();
            }
        } else if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer) {
            self.parse_json(&value);
        }
    }

    fn consume_sse_events(&mut self) {
        while let Some((boundary, delimiter)) = sse_boundary(&self.buffer) {
            let event = self
                .buffer
                .drain(..boundary + delimiter)
                .collect::<Vec<_>>();
            let mut data = Vec::new();
            for line in event.split(|byte| *byte == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some(value) = line.strip_prefix(b"data:") {
                    let value = value.strip_prefix(b" ").unwrap_or(value);
                    if !data.is_empty() {
                        data.push(b'\n');
                    }
                    data.extend_from_slice(value);
                }
            }
            if data == b"[DONE]" {
                if self.client == ClientKind::Claude && self.claude_tokens.total_tokens() > 0 {
                    self.record_claude();
                }
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&data) {
                self.parse_json(&value);
            }
        }
    }

    fn parse_json(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(message) = value.get("message") {
                    self.claude_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
                    self.claude_model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if let Some(usage) = message.get("usage") {
                        self.claude_tokens.input_tokens = number(usage, "input_tokens");
                        self.claude_tokens.cache_write_tokens =
                            number(usage, "cache_creation_input_tokens");
                        self.claude_tokens.cache_read_tokens =
                            number(usage, "cache_read_input_tokens");
                    }
                }
            }
            Some("message_delta") => {
                if let Some(usage) = value.get("usage") {
                    self.claude_tokens.output_tokens = number(usage, "output_tokens");
                }
                if value
                    .pointer("/delta/stop_reason")
                    .is_some_and(|value| !value.is_null())
                {
                    self.record_claude();
                }
            }
            Some("message_stop") => self.record_claude(),
            Some("response.completed") => {
                if let Some(response) = value.get("response") {
                    self.record_openai(response);
                }
            }
            _ => {
                if value.get("usage").is_some() {
                    self.record_openai(value);
                }
            }
        }
    }

    fn record_claude(&mut self) {
        if self.recorded || self.claude_tokens.total_tokens() == 0 {
            return;
        }
        self.claude_tokens.request_count = 1;
        let model = self
            .claude_model
            .clone()
            .or_else(|| self.fallback_model.clone())
            .unwrap_or_else(|| "unknown".into());
        let correlation = self.claude_id.as_ref().map_or_else(
            || token_correlation(self.client, &model, &self.claude_tokens),
            |id| digest(&format!("claude:{id}")),
        );
        let event = self.event(
            model,
            self.claude_tokens.clone(),
            correlation.clone(),
            correlation,
        );
        self.submit(event);
    }

    fn record_openai(&mut self, value: &Value) {
        if self.recorded {
            return;
        }
        let Some(usage) = value.get("usage") else {
            return;
        };
        let tokens = if self.client == ClientKind::Claude {
            UsageTokenSummary {
                input_tokens: number(usage, "input_tokens"),
                cache_write_tokens: number(usage, "cache_creation_input_tokens"),
                cache_read_tokens: number(usage, "cache_read_input_tokens"),
                output_tokens: number(usage, "output_tokens"),
                reasoning_output_tokens: 0,
                request_count: 1,
            }
        } else {
            openai_tokens(usage)
        };
        if tokens.total_tokens() == 0 {
            return;
        }
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.fallback_model.clone())
            .unwrap_or_else(|| "unknown".into());
        let correlation = token_correlation(self.client, &model, &tokens);
        let dedup = value.get("id").and_then(Value::as_str).map_or_else(
            || format!("proxy:{correlation}"),
            |id| digest(&format!("proxy:{}:{id}", self.client)),
        );
        self.submit(self.event(model, tokens, dedup, correlation));
    }

    fn event(
        &self,
        model: String,
        tokens: UsageTokenSummary,
        dedup_key: String,
        correlation_key: String,
    ) -> UsageEvent {
        UsageEvent {
            client: self.client,
            source: UsageDataSource::Proxy,
            provider_id: Some(self.provider_id.clone()),
            provider_name: self.provider_name.clone(),
            provider_revision: self.provider_revision,
            model,
            event_at: self.event_at,
            tokens,
            attribution: UsageAttribution::Exact,
            dedup_key,
            correlation_key: Some(correlation_key),
        }
    }

    fn submit(&mut self, event: UsageEvent) {
        self.recorded = true;
        let collector = self.collector.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                if let Err(error) = collector.insert_event(&event) {
                    tracing::warn!(code = error.code(), "proxy usage observation failed");
                }
            });
        } else if let Err(error) = collector.insert_event(&event) {
            tracing::warn!(code = error.code(), "proxy usage observation failed");
        }
    }
}

fn sse_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
        .or_else(|| {
            bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| (index, 4))
        })
}

fn collect_jsonl(root: &Path, client: ClientKind, output: &mut Vec<(ClientKind, PathBuf)>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_jsonl(&path, client, output);
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "jsonl") {
            output.push((client, path));
        }
    }
}

fn parse_claude_session(value: &Value) -> Option<UsageEvent> {
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let message_id = message.get("id")?.as_str()?;
    let model = message.get("model")?.as_str()?.to_owned();
    let event_at = timestamp(value)?;
    let tokens = UsageTokenSummary {
        input_tokens: number(usage, "input_tokens"),
        cache_write_tokens: number(usage, "cache_creation_input_tokens"),
        cache_read_tokens: number(usage, "cache_read_input_tokens"),
        output_tokens: number(usage, "output_tokens"),
        reasoning_output_tokens: 0,
        request_count: 1,
    };
    let correlation = digest(&format!("claude:{message_id}"));
    Some(UsageEvent {
        client: ClientKind::Claude,
        source: UsageDataSource::Session,
        provider_id: None,
        provider_name: "Unattributed".into(),
        provider_revision: 0,
        model,
        event_at,
        tokens,
        attribution: UsageAttribution::Unattributed,
        dedup_key: correlation.clone(),
        correlation_key: Some(correlation),
    })
}

fn parse_codex_session(
    value: &Value,
    current_model: &mut String,
    cumulative: &mut Option<UsageTokenSummary>,
) -> Option<UsageEvent> {
    if value.get("type").and_then(Value::as_str) == Some("turn_context")
        && let Some(model) = value.pointer("/payload/model").and_then(Value::as_str)
    {
        model.clone_into(current_model);
        return None;
    }
    if value.get("type")?.as_str()? != "event_msg"
        || value.pointer("/payload/type")?.as_str()? != "token_count"
    {
        return None;
    }
    let payload = value.get("payload")?;
    let last = payload
        .get("info")
        .and_then(|info| info.get("last_token_usage"))
        .or_else(|| payload.get("last_token_usage"))
        .filter(|usage| !usage.is_null());
    let total = payload
        .get("info")
        .and_then(|info| info.get("total_token_usage"))
        .or_else(|| payload.get("total_token_usage"))
        .filter(|usage| !usage.is_null());
    let tokens = if let Some(usage) = last {
        if let Some(total) = total {
            *cumulative = Some(openai_tokens(total));
        }
        openai_tokens(usage)
    } else {
        let total = total?;
        let current = openai_tokens(total);
        let delta = cumulative.as_ref().map_or_else(
            || current.clone(),
            |previous| subtract_tokens(&current, previous),
        );
        *cumulative = Some(current);
        delta
    };
    if tokens.total_tokens() == 0 {
        return None;
    }
    let event_at = timestamp(value)?;
    let correlation = token_correlation(ClientKind::Codex, current_model, &tokens);
    Some(UsageEvent {
        client: ClientKind::Codex,
        source: UsageDataSource::Session,
        provider_id: None,
        provider_name: "Unattributed".into(),
        provider_revision: 0,
        model: current_model.clone(),
        event_at,
        tokens,
        attribution: UsageAttribution::Unattributed,
        dedup_key: String::new(),
        correlation_key: Some(correlation),
    })
}

pub(crate) fn openai_tokens(usage: &Value) -> UsageTokenSummary {
    let total_input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_read = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| usage.get("cached_input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = usage
        .pointer("/input_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    UsageTokenSummary {
        input_tokens: total_input
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        cache_write_tokens: cache_write,
        cache_read_tokens: cache_read,
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .or_else(|| usage.get("completion_tokens").and_then(Value::as_u64))
            .unwrap_or(0),
        reasoning_output_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
            })
            .or_else(|| usage.get("reasoning_output_tokens").and_then(Value::as_u64))
            .unwrap_or(0),
        request_count: 1,
    }
}

fn subtract_tokens(current: &UsageTokenSummary, previous: &UsageTokenSummary) -> UsageTokenSummary {
    if current.input_tokens < previous.input_tokens
        || current.cache_write_tokens < previous.cache_write_tokens
        || current.cache_read_tokens < previous.cache_read_tokens
        || current.output_tokens < previous.output_tokens
        || current.reasoning_output_tokens < previous.reasoning_output_tokens
    {
        return current.clone();
    }
    UsageTokenSummary {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        cache_write_tokens: current
            .cache_write_tokens
            .saturating_sub(previous.cache_write_tokens),
        cache_read_tokens: current
            .cache_read_tokens
            .saturating_sub(previous.cache_read_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        reasoning_output_tokens: current
            .reasoning_output_tokens
            .saturating_sub(previous.reasoning_output_tokens),
        request_count: 1,
    }
}

fn stored_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
    let attribution: String = row.get(10)?;
    Ok(StoredEvent {
        provider_id: row.get(0)?,
        provider_name: row.get(1)?,
        provider_revision: row.get(2)?,
        model: row.get(3)?,
        event_at: row.get(4)?,
        tokens: UsageTokenSummary {
            input_tokens: row.get(5)?,
            cache_write_tokens: row.get(6)?,
            cache_read_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            reasoning_output_tokens: row.get(9)?,
            request_count: 1,
        },
        attribution: match attribution.as_str() {
            "exact" => UsageAttribution::Exact,
            "inferred" => UsageAttribution::Inferred,
            _ => UsageAttribution::Unattributed,
        },
    })
}

fn filter_options(events: &[StoredEvent]) -> UsageFilterOptions {
    let mut providers = HashMap::<String, UsageProviderOption>::new();
    let mut models = BTreeSet::<String>::new();
    for event in events {
        if let Some(id) = &event.provider_id {
            providers
                .entry(id.clone())
                .and_modify(|option| {
                    option.inferred |= event.attribution == UsageAttribution::Inferred;
                })
                .or_insert_with(|| UsageProviderOption {
                    id: id.clone(),
                    name: event.provider_name.clone(),
                    inferred: event.attribution == UsageAttribution::Inferred,
                });
        }
        models.insert(event.model.clone());
    }
    let mut providers = providers.into_values().collect::<Vec<_>>();
    providers.sort_by_key(|provider| provider.name.to_lowercase());
    UsageFilterOptions {
        providers,
        models: models.into_iter().collect(),
    }
}

fn add_tokens(target: &mut UsageTokenSummary, value: &UsageTokenSummary) {
    target.input_tokens = target.input_tokens.saturating_add(value.input_tokens);
    target.cache_write_tokens = target
        .cache_write_tokens
        .saturating_add(value.cache_write_tokens);
    target.cache_read_tokens = target
        .cache_read_tokens
        .saturating_add(value.cache_read_tokens);
    target.output_tokens = target.output_tokens.saturating_add(value.output_tokens);
    target.reasoning_output_tokens = target
        .reasoning_output_tokens
        .saturating_add(value.reasoning_output_tokens);
    target.request_count = target.request_count.saturating_add(value.request_count);
}

fn timestamp(value: &Value) -> Option<i64> {
    value.get("timestamp").and_then(Value::as_i64).or_else(|| {
        value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp())
    })
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn token_correlation(client: ClientKind, model: &str, tokens: &UsageTokenSummary) -> String {
    digest(&format!(
        "{client}:{model}:{}:{}:{}:{}:{}",
        tokens.input_tokens,
        tokens.cache_write_tokens,
        tokens.cache_read_tokens,
        tokens.output_tokens,
        tokens.reasoning_output_tokens
    ))
}

fn date_range(from: i64, to: i64) -> Vec<String> {
    let Some(mut date) = DateTime::<Utc>::from_timestamp(from, 0)
        .map(|value| value.with_timezone(&Local).date_naive())
    else {
        return Vec::new();
    };
    let Some(end) = DateTime::<Utc>::from_timestamp(to.saturating_sub(1), 0)
        .map(|value| value.with_timezone(&Local).date_naive())
    else {
        return Vec::new();
    };
    let mut dates = Vec::new();
    while date <= end && dates.len() < 92 {
        dates.push(date.format("%Y-%m-%d").to_string());
        date += chrono::Duration::days(1);
    }
    dates
}

fn local_date(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0).map_or_else(
        || "unknown".into(),
        |value| value.with_timezone(&Local).format("%Y-%m-%d").to_string(),
    )
}

fn retention_cutoff() -> i64 {
    let today = Local::now().date_naive();
    let cutoff = today - chrono::Duration::days(RETENTION_DAYS - 1);
    cutoff
        .and_hms_opt(0, 0, 0)
        .and_then(|value| value.and_local_timezone(Local).earliest())
        .map_or(0, |value| value.timestamp())
}

fn last_complete_offset(path: &Path, size: u64) -> Result<u64> {
    if size == 0 {
        return Ok(0);
    }
    let mut file = File::open(path)?;
    let read_size = size.min(64 * 1024);
    file.seek(SeekFrom::End(-i64::try_from(read_size).unwrap_or(i64::MAX)))?;
    let mut bytes = Vec::with_capacity(usize::try_from(read_size).unwrap_or(64 * 1024));
    file.read_to_end(&mut bytes)?;
    Ok(bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| size - read_size + index as u64 + 1))
}

fn tail_fingerprint(path: &Path, offset: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let start = offset.saturating_sub(128);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; usize::try_from(offset - start).unwrap_or(128)];
    file.read_exact(&mut bytes)?;
    Ok(digest_bytes(&bytes))
}

fn path_hash(path: &Path) -> String {
    let normalized = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    digest(&normalized.to_string_lossy())
}

fn digest(value: &str) -> String {
    digest_bytes(value.as_bytes())
}

fn digest_bytes(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn modified_seconds(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| {
            i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
        })
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| {
            i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::model::{AuthScheme, ProviderInput, ProviderProxyConfig, ProviderScope};
    use hsin_core::CodexImageConfig;

    fn test_collector() -> (PathBuf, Arc<Database>, Arc<UsageCollector>) {
        let root = std::env::temp_dir().join(format!("hsind-usage-{}", uuid::Uuid::new_v4()));
        let codex = root.join("codex");
        let claude = root.join("claude");
        fs::create_dir_all(codex.join("sessions")).expect("Codex session directory");
        fs::create_dir_all(claude.join("projects/test")).expect("Claude session directory");
        let db = Arc::new(
            Database::open(&root.join("hsin.sqlite3"), &root.join("backups")).expect("database"),
        );
        let collector = UsageCollector::new(
            db.clone(),
            &codex.join("config.toml"),
            &claude.join("settings.json"),
        );
        (root, db, collector)
    }

    fn claude_line(id: &str, output: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": Utc::now().to_rfc3339(),
            "message": {
                "id": id,
                "model": "claude-test",
                "usage": {
                    "input_tokens": 2,
                    "cache_creation_input_tokens": 3,
                    "cache_read_input_tokens": 5,
                    "output_tokens": output
                }
            }
        })
        .to_string()
    }

    #[test]
    fn openai_input_excludes_cached_tokens() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 60, "cache_write_tokens": 10},
            "output_tokens": 20,
            "output_tokens_details": {"reasoning_tokens": 8}
        });
        let tokens = openai_tokens(&usage);
        assert_eq!(tokens.input_tokens, 30);
        assert_eq!(tokens.cache_read_tokens, 60);
        assert_eq!(tokens.cache_write_tokens, 10);
        assert_eq!(tokens.total_tokens(), 120);
        assert_eq!(tokens.reasoning_output_tokens, 8);

        let compatible = openai_tokens(&serde_json::json!({
            "prompt_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 20},
            "completion_tokens": 9,
            "completion_tokens_details": {"reasoning_tokens": 4}
        }));
        assert_eq!(compatible.input_tokens, 30);
        assert_eq!(compatible.cache_read_tokens, 20);
        assert_eq!(compatible.output_tokens, 9);
        assert_eq!(compatible.reasoning_output_tokens, 4);
    }

    #[test]
    fn claude_message_ids_are_hashed() {
        let event = parse_claude_session(&serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-09-14T00:00:00Z",
            "message": {
                "id": "msg-sensitive",
                "model": "claude-test",
                "usage": {"input_tokens": 2, "cache_creation_input_tokens": 3, "cache_read_input_tokens": 5, "output_tokens": 7}
            }
        })).expect("event");
        assert!(!event.dedup_key.contains("msg-sensitive"));
        assert_eq!(event.tokens.total_tokens(), 17);
    }

    #[test]
    fn first_start_skips_existing_lines_then_imports_complete_appends() {
        let (root, db, collector) = test_collector();
        let log = root.join("claude/projects/test/session.jsonl");
        fs::write(&log, format!("{}\n", claude_line("old", 7))).expect("old log");
        collector.initialize().expect("initialize");
        let stored_path: String = db
            .connection
            .lock()
            .query_row(
                "SELECT path_hash FROM usage_sync_cursors LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("hashed cursor path");
        assert_eq!(stored_path.len(), 64);
        assert!(!stored_path.contains("projects"));
        let now = unix_time();
        let query = UsageStatsQuery {
            client: ClientKind::Claude,
            from: now - 10,
            to: now + 10,
            provider_id: None,
            model: None,
        };
        collector.sync().expect("initial sync");
        assert_eq!(
            collector
                .query(query.clone())
                .expect("query")
                .summary
                .request_count,
            0
        );

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("append");
        write!(file, "{}", claude_line("new", 7)).expect("partial line");
        file.flush().expect("flush");
        collector.sync().expect("partial sync");
        assert_eq!(
            collector
                .query(query.clone())
                .expect("query")
                .summary
                .request_count,
            0
        );
        writeln!(file).expect("complete line");
        file.flush().expect("flush");
        collector.sync().expect("complete sync");
        let report = collector.query(query).expect("query");
        assert_eq!(report.summary.request_count, 1);
        assert_eq!(report.summary.total_tokens(), 17);
        assert_eq!(report.attribution.unattributed, 1);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn repeated_claude_message_uses_max_output_and_route_snapshot() {
        let (root, db, collector) = test_collector();
        let provider = db
            .add_provider(&ProviderInput {
                client: ClientKind::Claude,
                name: "Historical route".into(),
                description: String::new(),
                base_url: "https://example.test".into(),
                auth_scheme: AuthScheme::XApiKey,
                model: None,
                codex_config_name: None,
                claude_model_mapping: None,
                scope: ProviderScope::Primary,
                codex_image: CodexImageConfig::default(),
                codex_tuning: hsin_core::CodexTuningSettings::default(),
                network_proxy: ProviderProxyConfig::default(),
            })
            .expect("provider");
        db.set_active(ClientKind::Claude, &provider.id, "synchronized")
            .expect("active provider");
        collector.initialize().expect("initialize");
        let log = root.join("claude/projects/test/new.jsonl");
        fs::write(
            &log,
            format!(
                "{}\r\n{}\r\n",
                claude_line("same", 1),
                claude_line("same", 7)
            ),
        )
        .expect("log");
        collector.sync().expect("sync");
        let now = unix_time();
        let report = collector
            .query(UsageStatsQuery {
                client: ClientKind::Claude,
                from: now - 10,
                to: now + 10,
                provider_id: Some(provider.id.clone()),
                model: Some("claude-test".into()),
            })
            .expect("query");
        assert_eq!(report.summary.request_count, 1);
        assert_eq!(report.summary.output_tokens, 7);
        assert_eq!(report.attribution.inferred, 1);
        assert_eq!(report.providers[0].provider_name, "Historical route");
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn proxy_claude_sse_is_exact_and_promotes_session_usage() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let correlation = digest("claude:msg-1");
        collector
            .insert_event(&UsageEvent {
                client: ClientKind::Claude,
                source: UsageDataSource::Session,
                provider_id: None,
                provider_name: "Unattributed".into(),
                provider_revision: 0,
                model: "claude-test".into(),
                event_at: unix_time(),
                tokens: UsageTokenSummary {
                    input_tokens: 2,
                    cache_write_tokens: 3,
                    cache_read_tokens: 5,
                    output_tokens: 7,
                    reasoning_output_tokens: 0,
                    request_count: 1,
                },
                attribution: UsageAttribution::Unattributed,
                dedup_key: correlation.clone(),
                correlation_key: Some(correlation),
            })
            .expect("session event");
        let mut observer = collector.proxy_observer(
            ClientKind::Claude,
            "provider-1".into(),
            "Proxy route".into(),
            4,
            None,
            true,
        );
        observer.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":5}}}\n\n");
        observer.feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n");
        observer.finish();
        for _ in 0..20 {
            tokio::task::yield_now().await;
            let now = unix_time();
            let report = collector
                .query(UsageStatsQuery {
                    client: ClientKind::Claude,
                    from: now - 10,
                    to: now + 10,
                    provider_id: None,
                    model: None,
                })
                .expect("query");
            if report.attribution.exact == 1 {
                assert_eq!(report.attribution.exact, 1);
                assert_eq!(report.summary.request_count, 1);
                assert_eq!(report.summary.total_tokens(), 17);
                assert_eq!(report.providers[0].provider_revision, 4);
                drop(collector);
                drop(db);
                fs::remove_dir_all(root).expect("cleanup");
                return;
            }
        }
        panic!("proxy observation was not committed");
    }

    #[test]
    fn codex_session_usage_is_attributed_to_an_official_route() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        db.connection
            .lock()
            .execute(
                "UPDATE usage_routes SET provider_id='official-codex',provider_name='Official',provider_revision=1 WHERE client='codex'",
                [],
            )
            .expect("official route");
        let timestamp = Utc::now().to_rfc3339();
        let log = root.join("codex/sessions/new.jsonl");
        fs::write(
            &log,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"turn_context","timestamp":timestamp,"payload":{"model":"gpt-official"}}),
                serde_json::json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"reasoning_output_tokens":8}}}})
            ),
        )
        .expect("Codex log");
        collector.sync().expect("sync");
        let now = unix_time();
        let report = collector
            .query(UsageStatsQuery {
                client: ClientKind::Codex,
                from: now - 10,
                to: now + 10,
                provider_id: Some("official-codex".into()),
                model: None,
            })
            .expect("query");
        assert_eq!(report.summary.input_tokens, 40);
        assert_eq!(report.summary.cache_read_tokens, 60);
        assert_eq!(report.summary.output_tokens, 20);
        assert_eq!(report.summary.reasoning_output_tokens, 8);
        assert_eq!(report.attribution.inferred, 1);
        assert_eq!(report.providers[0].provider_name, "Official");
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn codex_cumulative_usage_handles_duplicates_and_resets() {
        let first = UsageTokenSummary {
            input_tokens: 100,
            output_tokens: 20,
            ..UsageTokenSummary::default()
        };
        assert_eq!(subtract_tokens(&first, &first).total_tokens(), 0);
        let reset = UsageTokenSummary {
            input_tokens: 5,
            output_tokens: 2,
            ..UsageTokenSummary::default()
        };
        assert_eq!(subtract_tokens(&reset, &first), reset);
    }

    #[test]
    fn cleanup_removes_expired_events_without_deleting_cursors() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let event = |dedup_key: &str, event_at: i64| UsageEvent {
            client: ClientKind::Codex,
            source: UsageDataSource::Session,
            provider_id: None,
            provider_name: "Unattributed".into(),
            provider_revision: 0,
            model: "gpt-test".into(),
            event_at,
            tokens: UsageTokenSummary {
                input_tokens: 1,
                request_count: 1,
                ..UsageTokenSummary::default()
            },
            attribution: UsageAttribution::Unattributed,
            dedup_key: dedup_key.into(),
            correlation_key: None,
        };
        collector
            .insert_event(&event("expired", retention_cutoff() - 1))
            .expect("expired event");
        collector
            .insert_event(&event("recent", unix_time()))
            .expect("recent event");
        let cursor_count_before: u64 = db
            .connection
            .lock()
            .query_row("SELECT count(*) FROM usage_sync_cursors", [], |row| {
                row.get(0)
            })
            .expect("cursor count");
        collector
            .cleanup_if_due(unix_time(), true)
            .expect("cleanup");
        let event_count: u64 = db
            .connection
            .lock()
            .query_row("SELECT count(*) FROM usage_events", [], |row| row.get(0))
            .expect("event count");
        let cursor_count_after: u64 = db
            .connection
            .lock()
            .query_row("SELECT count(*) FROM usage_sync_cursors", [], |row| {
                row.get(0)
            })
            .expect("cursor count");
        assert_eq!(event_count, 1);
        assert_eq!(cursor_count_after, cursor_count_before);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }
}
