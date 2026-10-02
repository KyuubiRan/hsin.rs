use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use hsin_core::{
    ClientKind, ConnectionMode, ModelPrice, USAGE_CALENDAR_DAYS, USAGE_MODEL_SERIES_DAYS,
    UsageAttribution, UsageAttributionCounts, UsageCalendarDay, UsageCost, UsageDailyBucket,
    UsageDataSource, UsageFilterOptions, UsageModelBreakdown, UsageOverview, UsageProjection,
    UsageProviderBreakdown, UsageProviderOption, UsageQuotaCapacity, UsageQuotaCycle,
    UsageQuotaEstimate, UsageStatsQuery, UsageStatsReport, UsageSyncResult, UsageTokenSummary,
    add_usage_cost, best_model_price,
};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{db::Database, error::Result};

const STARTED_AT_KEY: &str = "usage_started_at";
const LAST_SYNCED_AT_KEY: &str = "usage_last_synced_at";
const SYNC_STATUS_KEY: &str = "usage_sync_status";
const LAST_CLEANUP_AT_KEY: &str = "usage_last_cleanup_at";
const ROLLUP_VERSION_KEY: &str = "usage_rollup_version";
const ROLLUP_VERSION: &str = "1";
const CODEX_REPAIR_KEY: &str = "usage_codex_repair_version";
/// Version 2 also fills in the account and session provider of stored quota readings; version 3
/// reaches back over the longer quota history.
const CODEX_REPAIR_VERSION: &str = "3";
/// Cycles listed per quota window.
const QUOTA_CYCLES_SHOWN: usize = 6;
/// How far back quota readings are kept and repaired. The screen shows the last 30 days by default
/// and older plans on request.
const QUOTA_HISTORY_DAYS: i64 = 90;
/// Claude Code keeps its plan usage in this file, beside or one level above its settings.
const CLAUDE_GLOBAL_CONFIG: &str = ".claude.json";
/// The global config also holds project history; anything larger is not read.
const MAX_CLAUDE_CONFIG_BYTES: u64 = 64 * 1024 * 1024;
/// Readings whose reset times differ by more than this belong to different cycles.
const QUOTA_CYCLE_TOLERANCE_SECONDS: i64 = 600;
const UNKNOWN_MODEL: &str = "unknown";
/// The longest daily series a query returns, so no range can outgrow an IPC frame.
const MAX_QUERY_DAYS: i64 = 3660;
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
    /// What the parser knew at `byte_offset`, so a resumed read keeps the current model.
    parser_state: String,
}

/// Codex writes the model once per turn and usage per request, so the parser carries both
/// across lines — and, through the cursor, across syncs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CodexState {
    model: String,
    cumulative: Option<UsageTokenSummary>,
    /// The session's provider, from `session_meta`: `openai` for a `ChatGPT` login.
    #[serde(default)]
    source: String,
    /// A hash of the session's account, never the account ID itself.
    #[serde(default)]
    account: String,
}

impl Default for CodexState {
    fn default() -> Self {
        Self {
            model: UNKNOWN_MODEL.into(),
            cumulative: None,
            source: String::new(),
            account: String::new(),
        }
    }
}

impl CodexState {
    fn restore(saved: &str) -> Self {
        serde_json::from_str(saved).unwrap_or_default()
    }
}

/// One quota window as a Codex `token_count` line reports it.
#[derive(Debug, Clone, PartialEq)]
struct QuotaReading {
    window: &'static str,
    limit_id: String,
    window_minutes: i64,
    resets_at: i64,
    used_percent: f64,
    plan_type: Option<String>,
}

#[derive(Debug, Clone)]
struct QuotaRow {
    account: String,
    source: String,
    window: String,
    limit_id: String,
    window_minutes: i64,
    resets_at: i64,
    used_percent: f64,
    plan_type: Option<String>,
    observed_at: i64,
    /// Usage since the previous reading of the series, by model.
    usage: Vec<(String, UsageTokenSummary)>,
    /// The part of the meter's movement the client itself caused, `0..=1`.
    share: f64,
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
/// Account, session provider, plan, limit, window name and length.
type QuotaGroupKey = (String, String, String, String, String, i64);

/// One stored hour of rolled-up usage for one provider revision and model.
#[derive(Debug, Clone)]
struct RollupRow {
    day: NaiveDate,
    hour: u8,
    provider_id: Option<String>,
    provider_name: String,
    provider_revision: u64,
    model: String,
    tokens: UsageTokenSummary,
    /// Requests attributed exactly, inferred, and unattributed.
    attribution: [u64; 3],
}

#[derive(Debug, Default)]
struct RollupGroup {
    provider_id: Option<String>,
    tokens: UsageTokenSummary,
    attribution: [u64; 3],
}

/// Resolves each provider and model to its price once per query.
struct PriceCache<'a> {
    prices: &'a [ModelPrice],
    resolved: HashMap<(Option<String>, String), Option<&'a ModelPrice>>,
}

impl<'a> PriceCache<'a> {
    fn new(prices: &'a [ModelPrice]) -> Self {
        Self {
            prices,
            resolved: HashMap::new(),
        }
    }

    fn cost(&mut self, row: &RollupRow) -> Option<(String, f64)> {
        let prices = self.prices;
        let price = *self
            .resolved
            .entry((row.provider_id.clone(), row.model.clone()))
            .or_insert_with(|| best_model_price(prices, row.provider_id.as_deref(), &row.model));
        price.map(|price| (price.currency.clone(), price.cost(&row.tokens)))
    }
}

pub(crate) struct UsageCollector {
    db: Arc<Database>,
    codex_home: PathBuf,
    claude_home: PathBuf,
    sync_lock: Mutex<()>,
    /// Modification time and size of the Claude global config when it was last read.
    claude_config_seen: Mutex<Option<(i64, u64)>>,
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
            claude_config_seen: Mutex::new(None),
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
                let parser_state = if client == ClientKind::Codex {
                    serde_json::to_string(&codex_state_at(&path, offset).unwrap_or_default())?
                } else {
                    String::new()
                };
                self.save_cursor(
                    &path_hash(&path),
                    client,
                    &Cursor {
                        modified_at: modified_seconds(&metadata),
                        file_size: metadata.len(),
                        byte_offset: offset,
                        tail_fingerprint: fingerprint,
                        parser_state,
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
        // Claude five-hour readings an earlier version stored.
        self.db.connection.lock().execute(
            "DELETE FROM usage_quota_readings WHERE client='claude' AND quota_window='primary'",
            [],
        )?;
        self.backfill_rollups(now)
    }

    /// Runs the one-time Codex repair; the daemon calls it in the background after startup, since
    /// rereading weeks of sessions must not hold up IPC.
    pub(crate) fn repair_legacy_codex_usage(&self) -> Result<()> {
        self.repair_codex_sessions(unix_time())
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
        if self.sync_claude_quota(now).is_err() {
            result.failed_files = result.failed_files.saturating_add(1);
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
        self.refresh_rollups()?;
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

    #[allow(clippy::too_many_lines)]
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
            parser_state: String::new(),
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
        let mut codex = if cursor.byte_offset > 0 {
            CodexState::restore(&cursor.parser_state)
        } else {
            CodexState::default()
        };
        let quota_horizon = unix_time() - QUOTA_HISTORY_DAYS * 86_400;
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
                    let parsed = parse_codex_session(&value, &mut codex);
                    self.record_quota_readings(
                        &value,
                        &quota_reading_key(&hash, line_start),
                        parsed.as_ref(),
                        &codex,
                        quota_horizon,
                    )?;
                    parsed
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
                parsed.dedup_key = session_event_key(&parsed, &hash, line_start);
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
                parser_state: if client == ClientKind::Codex {
                    serde_json::to_string(&codex)?
                } else {
                    String::new()
                },
            },
            unix_time(),
        )
    }

    /// Stores the quota windows a Codex line reports, with the usage the same line carries, so the
    /// estimate can relate meter movement to tokens.
    fn record_quota_readings(
        &self,
        value: &Value,
        key: &str,
        event: Option<&UsageEvent>,
        state: &CodexState,
        horizon: i64,
    ) -> Result<()> {
        let Some(observed_at) = timestamp(value) else {
            return Ok(());
        };
        let readings = codex_quota_readings(value, observed_at);
        if readings.is_empty() || observed_at < horizon {
            return Ok(());
        }
        let tokens = event.map(|event| event.tokens.clone()).unwrap_or_default();
        let connection = self.db.connection.lock();
        for reading in readings {
            connection.execute(
                "INSERT INTO usage_quota_readings(reading_key,quota_window,client,observed_at,limit_id,window_minutes,resets_at,used_percent,plan_type,model,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,account,session_provider) VALUES(?1,?2,'codex',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) ON CONFLICT(reading_key,quota_window) DO UPDATE SET model=excluded.model,account=excluded.account,session_provider=excluded.session_provider",
                params![
                    key, reading.window, observed_at, reading.limit_id, reading.window_minutes,
                    reading.resets_at, reading.used_percent, reading.plan_type, state.model,
                    tokens.input_tokens, tokens.cache_write_tokens, tokens.cache_read_tokens,
                    tokens.output_tokens, state.account, state.source,
                ],
            )?;
        }
        Ok(())
    }

    /// Before parser state was kept across syncs, a sync that resumed mid-file lost the current
    /// model and filed the rest of the session under `unknown`. This rereads recent Codex sessions
    /// once, relabels those events, fills in the cursors' parser state, and collects the quota
    /// readings of the files it passes.
    fn repair_codex_sessions(&self, now: i64) -> Result<()> {
        if self.meta(CODEX_REPAIR_KEY)?.as_deref() == Some(CODEX_REPAIR_VERSION) {
            return Ok(());
        }
        let horizon = now - QUOTA_HISTORY_DAYS * 86_400;
        for (client, path) in self.session_files() {
            let recent =
                fs::metadata(&path).is_ok_and(|metadata| modified_seconds(&metadata) >= horizon);
            if client == ClientKind::Codex && recent {
                // Per file, so a sync waits for one file at most and never races this pass on the
                // same cursor. One unreadable file must not keep the others unrepaired.
                let _guard = self.sync_lock.lock();
                let _ = self.repair_codex_file(&path, horizon, now);
            }
        }
        self.refresh_rollups()?;
        self.set_meta(CODEX_REPAIR_KEY, CODEX_REPAIR_VERSION, now)
    }

    fn repair_codex_file(&self, path: &Path, horizon: i64, now: i64) -> Result<()> {
        let hash = path_hash(path);
        let cursor = self.cursor(&hash)?;
        let mut state = CodexState::default();
        let mut state_at_cursor = None;
        let mut offset = 0_u64;
        let mut reader = BufReader::new(File::open(path)?);
        loop {
            if cursor
                .as_ref()
                .is_some_and(|cursor| cursor.byte_offset == offset)
            {
                state_at_cursor = Some(state.clone());
            }
            let line_start = offset;
            let mut bytes = Vec::new();
            let read = reader.read_until(b'\n', &mut bytes)?;
            if read == 0 || !bytes.ends_with(b"\n") {
                break;
            }
            offset = offset.saturating_add(read as u64);
            let Some(value) = codex_line(&bytes) else {
                continue;
            };
            let event = parse_codex_session(&value, &mut state);
            self.record_quota_readings(
                &value,
                &quota_reading_key(&hash, line_start),
                event.as_ref(),
                &state,
                horizon,
            )?;
            if let Some(event) = event
                && event.model != UNKNOWN_MODEL
            {
                self.relabel_unknown_event(&event, &hash, line_start)?;
            }
        }
        if let (Some(mut cursor), Some(state)) = (cursor, state_at_cursor) {
            cursor.parser_state = serde_json::to_string(&state)?;
            self.save_cursor(&hash, ClientKind::Codex, &cursor, now)?;
        }
        Ok(())
    }

    /// Moves an event stored under the `unknown` model to its real one. When the real one is
    /// already stored, the `unknown` copy is a duplicate and goes.
    fn relabel_unknown_event(&self, event: &UsageEvent, hash: &str, line_start: u64) -> Result<()> {
        let unknown = UsageEvent {
            model: UNKNOWN_MODEL.into(),
            correlation_key: Some(token_correlation(
                ClientKind::Codex,
                UNKNOWN_MODEL,
                &event.tokens,
            )),
            ..event.clone()
        };
        let unknown_key = session_event_key(&unknown, hash, line_start);
        let correct_key = session_event_key(event, hash, line_start);
        let mut connection = self.db.connection.lock();
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM usage_events WHERE dedup_key=?1)",
            [&unknown_key],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        let transaction = connection.transaction()?;
        let moved = transaction.execute(
            "UPDATE OR IGNORE usage_events SET model=?1,correlation_key=?2,dedup_key=?3 WHERE dedup_key=?4",
            params![event.model, event.correlation_key, correct_key, unknown_key],
        )?;
        if moved == 0 {
            transaction.execute(
                "DELETE FROM usage_events WHERE dedup_key=?1",
                [&unknown_key],
            )?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO usage_rollup_dirty(client,day) VALUES('codex',?1)",
            [local_date(event.event_at)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Back-calculates each quota window's allowance from the readings of its recent cycles.
    fn quota_estimates(
        &self,
        client: ClientKind,
        prices: &[ModelPrice],
        now: i64,
    ) -> Result<Vec<UsageQuotaEstimate>> {
        let rows = {
            let connection = self.db.connection.lock();
            let mut statement = connection.prepare(
                "SELECT quota_window,limit_id,window_minutes,resets_at,used_percent,plan_type,model,observed_at,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,account,session_provider,client_share FROM usage_quota_readings WHERE client=?1 AND observed_at>=?2 ORDER BY observed_at,rowid",
            )?;
            let rows = statement.query_map(
                params![client.to_string(), now - QUOTA_HISTORY_DAYS * 86_400],
                |row| {
                    Ok(QuotaRow {
                        account: row.get(12)?,
                        source: row.get(13)?,
                        window: row.get(0)?,
                        limit_id: row.get(1)?,
                        window_minutes: row.get(2)?,
                        resets_at: row.get(3)?,
                        used_percent: row.get(4)?,
                        plan_type: row.get(5)?,
                        observed_at: row.get(7)?,
                        usage: vec![(
                            row.get(6)?,
                            UsageTokenSummary {
                                input_tokens: row.get(8)?,
                                cache_write_tokens: row.get(9)?,
                                cache_read_tokens: row.get(10)?,
                                output_tokens: row.get(11)?,
                                reasoning_output_tokens: 0,
                                request_count: 1,
                            },
                        )],
                        share: row.get::<_, f64>(14)?.clamp(0.0, 1.0),
                    })
                },
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        // Different accounts, session providers and plans have different allowances, and a user
        // can move between them, so each gets its own series of readings.
        let mut rows = rows;
        if client == ClientKind::Claude {
            self.attach_claude_usage(&mut rows, now)?;
        }
        // Older clients do not name the account. When a provider and plan have exactly one known
        // account, their unnamed readings are that account's; otherwise they stay apart.
        let mut accounts = HashMap::<(String, Option<String>), BTreeSet<String>>::new();
        for row in rows.iter().filter(|row| !row.account.is_empty()) {
            accounts
                .entry((row.source.clone(), row.plan_type.clone()))
                .or_default()
                .insert(row.account.clone());
        }
        for row in rows.iter_mut().filter(|row| row.account.is_empty()) {
            if let Some(known) = accounts.get(&(row.source.clone(), row.plan_type.clone()))
                && let [account] = &known.iter().collect::<Vec<_>>()[..]
            {
                row.account.clone_from(*account);
            }
        }
        let mut groups = BTreeMap::<QuotaGroupKey, Vec<QuotaRow>>::new();
        for row in rows {
            groups
                .entry((
                    row.account.clone(),
                    row.source.clone(),
                    row.plan_type.clone().unwrap_or_default(),
                    row.limit_id.clone(),
                    row.window.clone(),
                    row.window_minutes,
                ))
                .or_default()
                .push(row);
        }
        let newest = groups
            .values()
            .filter_map(|rows| rows.last().map(|row| row.observed_at))
            .max()
            .unwrap_or(0);
        let today = Local::now().date_naive();
        let mut estimates = groups
            .values()
            .filter_map(|rows| {
                let mut estimate = estimate_quota(rows, prices, now, today)?;
                // What the client reports now; older windows belong to a plan or account that is
                // no longer in use, and their remaining share means nothing.
                estimate.current = estimate.observed_at >= newest - 3_600;
                if !estimate.current {
                    estimate.remaining = None;
                }
                Some(estimate)
            })
            .collect::<Vec<_>>();
        estimates.sort_by_key(|estimate| {
            (
                std::cmp::Reverse(estimate.current),
                std::cmp::Reverse(estimate.observed_at),
            )
        });
        Ok(estimates)
    }

    /// Claude Code refreshes its plan usage into its global config now and then. Each refresh is a
    /// reading; only the cached usage block and the account's plan are read, never credentials.
    fn sync_claude_quota(&self, now: i64) -> Result<()> {
        let Some(path) = self.claude_global_config() else {
            return Ok(());
        };
        let metadata = fs::metadata(&path)?;
        let seen = (modified_seconds(&metadata), metadata.len());
        if *self.claude_config_seen.lock() == Some(seen) {
            return Ok(());
        }
        *self.claude_config_seen.lock() = Some(seen);
        if metadata.len() > MAX_CLAUDE_CONFIG_BYTES {
            return Ok(());
        }
        let config: Value = serde_json::from_slice(&fs::read(&path)?)?;
        let Some(reading) = claude_quota_reading(&config) else {
            return Ok(());
        };
        if reading.observed_at < now - QUOTA_HISTORY_DAYS * 86_400 {
            return Ok(());
        }
        let key = digest(&format!(
            "claude-quota:{}:{}",
            reading.account, reading.observed_at
        ));
        let connection = self.db.connection.lock();
        for window in &reading.windows {
            connection.execute(
                "INSERT OR IGNORE INTO usage_quota_readings(reading_key,quota_window,client,observed_at,limit_id,window_minutes,resets_at,used_percent,plan_type,model,account,session_provider,client_share) VALUES(?1,?2,'claude',?3,'claude',?4,?5,?6,?7,'',?8,'anthropic',?9)",
                params![
                    key, window.window, reading.observed_at, window.window_minutes,
                    window.resets_at, window.used_percent, reading.plan_type, reading.account,
                    window.share,
                ],
            )?;
        }
        Ok(())
    }

    fn claude_global_config(&self) -> Option<PathBuf> {
        [
            Some(self.claude_home.join(CLAUDE_GLOBAL_CONFIG)),
            self.claude_home
                .parent()
                .map(|parent| parent.join(CLAUDE_GLOBAL_CONFIG)),
        ]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
    }

    /// Claude readings carry no usage of their own: each is credited with the official-login
    /// requests since the previous reading of its series. Requests through a relay are not the
    /// plan's and are left out.
    fn attach_claude_usage(&self, rows: &mut [QuotaRow], now: i64) -> Result<()> {
        let events = {
            let connection = self.db.connection.lock();
            let mut statement = connection.prepare(
                "SELECT model,event_at,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens FROM usage_events WHERE client='claude' AND provider_id=?1 AND event_at>=?2 ORDER BY event_at",
            )?;
            let rows = statement.query_map(
                params![
                    format!("official-{}", ClientKind::Claude.as_str()),
                    now - QUOTA_HISTORY_DAYS * 86_400
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        UsageTokenSummary {
                            input_tokens: row.get(2)?,
                            cache_write_tokens: row.get(3)?,
                            cache_read_tokens: row.get(4)?,
                            output_tokens: row.get(5)?,
                            reasoning_output_tokens: 0,
                            request_count: 1,
                        },
                    ))
                },
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut previous = HashMap::<(String, String), i64>::new();
        for row in rows.iter_mut() {
            let series = (row.account.clone(), row.window.clone());
            let since = previous.insert(series, row.observed_at);
            let Some(since) = since else {
                continue;
            };
            let mut usage = BTreeMap::<String, UsageTokenSummary>::new();
            let start = events.partition_point(|event| event.1 <= since);
            for (model, _, tokens) in events[start..]
                .iter()
                .take_while(|event| event.1 <= row.observed_at)
            {
                add_tokens(usage.entry(model.clone()).or_default(), tokens);
            }
            row.usage = usage.into_iter().collect();
        }
        Ok(())
    }

    /// Stores one event and marks the days it can have changed for the next rollup refresh, in one
    /// transaction so a rollup never misses an event.
    pub(crate) fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let mut connection = self.db.connection.lock();
        let transaction = connection.transaction()?;
        let mut days = BTreeSet::from([local_date(event.event_at)]);
        if event.source == UsageDataSource::Proxy
            && let Some(correlation) = &event.correlation_key
        {
            let removed = transaction.execute(
                "DELETE FROM usage_events WHERE id=(SELECT id FROM usage_events WHERE client=?1 AND source='session' AND correlation_key=?2 AND abs(event_at-?3)<=120 ORDER BY abs(event_at-?3) LIMIT 1)",
                params![event.client.to_string(), correlation, event.event_at],
            )?;
            if removed > 0 {
                days.insert(local_date(event.event_at - 120));
                days.insert(local_date(event.event_at + 120));
            }
        }
        let changed = transaction.execute(
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
        for day in days {
            transaction.execute(
                "INSERT OR IGNORE INTO usage_rollup_dirty(client,day) VALUES(?1,?2)",
                params![event.client.to_string(), day],
            )?;
        }
        transaction.commit()?;
        Ok(changed > 0)
    }

    /// Rebuilds the hourly rollups of every day an insert touched since the last call. Days whose
    /// raw events have aged out are left as they are: their rollups are the only record left.
    pub(crate) fn refresh_rollups(&self) -> Result<()> {
        let dirty = {
            let connection = self.db.connection.lock();
            let mut statement = connection.prepare("SELECT client,day FROM usage_rollup_dirty")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let oldest = local_date(retention_cutoff());
        for (client, day) in dirty {
            let bounds = (day >= oldest).then(|| local_day_bounds(&day)).flatten();
            let mut connection = self.db.connection.lock();
            let transaction = connection.transaction()?;
            if let Some((start, end)) = bounds {
                let events = {
                    let mut statement = transaction.prepare(
                        "SELECT provider_id,provider_name,provider_revision,model,event_at,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,reasoning_output_tokens,attribution FROM usage_events WHERE client=?1 AND event_at>=?2 AND event_at<?3",
                    )?;
                    let rows =
                        statement.query_map(params![client, start, end], stored_event_from_row)?;
                    rows.collect::<std::result::Result<Vec<_>, _>>()?
                };
                let mut groups = BTreeMap::<(u8, String, String, u64, String), RollupGroup>::new();
                for event in events {
                    let group = groups
                        .entry((
                            local_hour(event.event_at),
                            event.provider_id.clone().unwrap_or_default(),
                            event.provider_name.clone(),
                            event.provider_revision,
                            event.model.clone(),
                        ))
                        .or_insert_with(|| RollupGroup {
                            provider_id: event.provider_id.clone(),
                            ..RollupGroup::default()
                        });
                    add_tokens(&mut group.tokens, &event.tokens);
                    group.attribution[match event.attribution {
                        UsageAttribution::Exact => 0,
                        UsageAttribution::Inferred => 1,
                        UsageAttribution::Unattributed => 2,
                    }] += 1;
                }
                transaction.execute(
                    "DELETE FROM usage_hourly WHERE client=?1 AND day=?2",
                    params![client, day],
                )?;
                for ((hour, provider_key, provider_name, provider_revision, model), group) in groups
                {
                    transaction.execute(
                        "INSERT INTO usage_hourly(client,day,hour,provider_key,provider_id,provider_name,provider_revision,model,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,reasoning_output_tokens,request_count,exact_count,inferred_count,unattributed_count) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                        params![
                            client, day, hour, provider_key, group.provider_id, provider_name,
                            provider_revision, model, group.tokens.input_tokens,
                            group.tokens.cache_write_tokens, group.tokens.cache_read_tokens,
                            group.tokens.output_tokens, group.tokens.reasoning_output_tokens,
                            group.tokens.request_count, group.attribution[0],
                            group.attribution[1], group.attribution[2],
                        ],
                    )?;
                }
            }
            transaction.execute(
                "DELETE FROM usage_rollup_dirty WHERE client=?1 AND day=?2",
                params![client, day],
            )?;
            transaction.commit()?;
        }
        Ok(())
    }

    /// Rolls up events recorded before rollups existed, once.
    fn backfill_rollups(&self, now: i64) -> Result<()> {
        if self.meta(ROLLUP_VERSION_KEY)?.as_deref() == Some(ROLLUP_VERSION) {
            return Ok(());
        }
        {
            let mut connection = self.db.connection.lock();
            let transaction = connection.transaction()?;
            let days = {
                let mut statement =
                    transaction.prepare("SELECT client,event_at FROM usage_events")?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                rows.map(|row| row.map(|(client, at)| (client, local_date(at))))
                    .collect::<std::result::Result<BTreeSet<_>, _>>()?
            };
            for (client, day) in days {
                transaction.execute(
                    "INSERT OR IGNORE INTO usage_rollup_dirty(client,day) VALUES(?1,?2)",
                    params![client, day],
                )?;
            }
            transaction.commit()?;
        }
        self.refresh_rollups()?;
        self.set_meta(ROLLUP_VERSION_KEY, ROLLUP_VERSION, now)
    }

    fn rollups(
        &self,
        client: ClientKind,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<RollupRow>> {
        let connection = self.db.connection.lock();
        let mut statement = connection.prepare(
            "SELECT day,hour,provider_id,provider_name,provider_revision,model,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,reasoning_output_tokens,request_count,exact_count,inferred_count,unattributed_count FROM usage_hourly WHERE client=?1 AND day>=?2 AND day<=?3 ORDER BY day,hour",
        )?;
        let rows = statement.query_map(
            params![client.to_string(), date_key(from), date_key(to)],
            |row| {
                let day: String = row.get(0)?;
                Ok(RollupRow {
                    day: NaiveDate::parse_from_str(&day, "%Y-%m-%d").map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    hour: row.get(1)?,
                    provider_id: row.get(2)?,
                    provider_name: row.get(3)?,
                    provider_revision: row.get(4)?,
                    model: row.get(5)?,
                    tokens: UsageTokenSummary {
                        input_tokens: row.get(6)?,
                        cache_write_tokens: row.get(7)?,
                        cache_read_tokens: row.get(8)?,
                        output_tokens: row.get(9)?,
                        reasoning_output_tokens: row.get(10)?,
                        request_count: row.get(11)?,
                    },
                    attribution: [row.get(12)?, row.get(13)?, row.get(14)?],
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn first_rollup_day(&self, client: ClientKind) -> Result<Option<NaiveDate>> {
        let day: Option<String> = self.db.connection.lock().query_row(
            "SELECT min(day) FROM usage_hourly WHERE client=?1",
            [client.to_string()],
            |row| row.get(0),
        )?;
        Ok(day.and_then(|day| NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok()))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn query(&self, query: UsageStatsQuery) -> Result<UsageStatsReport> {
        if query.from >= query.to {
            return Err(crate::error::DaemonError::Invalid(
                "usage query start must be earlier than its end".into(),
            ));
        }
        self.refresh_rollups()?;
        let query_client = query.client;
        let today = Local::now().date_naive();
        let collected_since = self
            .meta(STARTED_AT_KEY)?
            .and_then(|value| value.parse().ok())
            .unwrap_or(query.from);
        let last_synced_at = self
            .meta(LAST_SYNCED_AT_KEY)?
            .and_then(|value| value.parse().ok());
        // Days before the first recorded one carry nothing, so the series starts there; that is
        // also what makes an all-time query (`from` = 0) a bounded one.
        let data_start = self
            .first_rollup_day(query.client)?
            .into_iter()
            .chain(local_naive_date(collected_since))
            .min()
            .unwrap_or(today)
            .min(today);
        let from = local_naive_date(query.from).map_or(data_start, |from| from.max(data_start));
        let to = local_naive_date(query.to.saturating_sub(1)).unwrap_or(today);
        let to = to.min(from + chrono::Duration::days(MAX_QUERY_DAYS));
        let all = if from <= to {
            self.rollups(query.client, from, to)?
        } else {
            Vec::new()
        };
        let price_rules = crate::pricing::all_prices(&self.db)?;
        let mut costs = PriceCache::new(&price_rules);

        let filters = filter_options(&all);
        let matches = |row: &&RollupRow| {
            query
                .provider_id
                .as_ref()
                .is_none_or(|id| row.provider_id.as_ref() == Some(id))
                && query.model.as_ref().is_none_or(|model| &row.model == model)
        };
        let mut summary = UsageTokenSummary::default();
        let mut cost = Vec::new();
        let mut unpriced_tokens = 0_u64;
        let mut daily = BTreeMap::<NaiveDate, UsageTokenSummary>::new();
        let mut hours = [0_u64; 24];
        let mut hourly = vec![UsageTokenSummary::default(); 24];
        let mut providers = HashMap::<
            (Option<String>, String, u64),
            (bool, UsageTokenSummary, Vec<UsageCost>),
        >::new();
        let mut models = HashMap::<
            String,
            (
                UsageTokenSummary,
                BTreeMap<NaiveDate, UsageTokenSummary>,
                Vec<UsageCost>,
            ),
        >::new();
        let mut attribution = UsageAttributionCounts::default();
        for row in all.iter().filter(matches) {
            add_tokens(&mut summary, &row.tokens);
            add_tokens(daily.entry(row.day).or_default(), &row.tokens);
            hours[usize::from(row.hour.min(23))] += row.tokens.total_tokens();
            add_tokens(&mut hourly[usize::from(row.hour.min(23))], &row.tokens);
            let priced = costs.cost(row);
            match &priced {
                Some((currency, amount)) => add_usage_cost(&mut cost, currency, *amount),
                None => unpriced_tokens += row.tokens.total_tokens(),
            }
            let provider = providers
                .entry((
                    row.provider_id.clone(),
                    row.provider_name.clone(),
                    row.provider_revision,
                ))
                .or_default();
            provider.0 |= row.attribution[1] > 0;
            add_tokens(&mut provider.1, &row.tokens);
            let model = models.entry(row.model.clone()).or_default();
            add_tokens(&mut model.0, &row.tokens);
            add_tokens(model.1.entry(row.day).or_default(), &row.tokens);
            if let Some((currency, amount)) = priced {
                add_usage_cost(&mut provider.2, &currency, amount);
                add_usage_cost(&mut model.2, &currency, amount);
            }
            attribution.exact += row.attribution[0];
            attribution.inferred += row.attribution[1];
            attribution.unattributed += row.attribution[2];
        }
        let dates = date_range(from, to);
        let series_dates = &dates[dates.len().saturating_sub(USAGE_MODEL_SERIES_DAYS)..];
        let favorite_model = models
            .iter()
            .max_by(|left, right| {
                (left.1.0.output_tokens, left.1.0.total_tokens(), right.0).cmp(&(
                    right.1.0.output_tokens,
                    right.1.0.total_tokens(),
                    left.0,
                ))
            })
            .map(|(model, _)| model.clone());
        let mut provider_rows = providers
            .into_iter()
            .map(
                |((provider_id, provider_name, provider_revision), (inferred, tokens, cost))| {
                    UsageProviderBreakdown {
                        provider_id,
                        provider_name,
                        provider_revision,
                        inferred,
                        tokens,
                        cost,
                    }
                },
            )
            .collect::<Vec<_>>();
        provider_rows.sort_by_key(|row| std::cmp::Reverse(row.tokens.total_tokens()));
        let mut model_rows = models
            .into_iter()
            .map(|(model, (tokens, daily, cost))| UsageModelBreakdown {
                model,
                tokens,
                daily: series_dates
                    .iter()
                    .map(|date| UsageDailyBucket {
                        date: date_key(*date),
                        tokens: daily.get(date).cloned().unwrap_or_default(),
                    })
                    .collect(),
                cost,
            })
            .collect::<Vec<_>>();
        model_rows.sort_by_key(|row| std::cmp::Reverse(row.tokens.total_tokens()));

        let calendar_from =
            today - chrono::Duration::days(i64::try_from(USAGE_CALENDAR_DAYS).unwrap_or(371) - 1);
        let mut calendar_days = BTreeMap::<NaiveDate, (UsageTokenSummary, Vec<UsageCost>)>::new();
        for row in self
            .rollups(query.client, calendar_from, today)?
            .iter()
            .filter(matches)
        {
            let day = calendar_days.entry(row.day).or_default();
            add_tokens(&mut day.0, &row.tokens);
            if let Some((currency, amount)) = costs.cost(row) {
                add_usage_cost(&mut day.1, &currency, amount);
            }
        }
        let calendar = date_range(calendar_from, today)
            .into_iter()
            .map(|date| {
                let (tokens, cost) = calendar_days.get(&date).cloned().unwrap_or_default();
                UsageCalendarDay {
                    date: date_key(date),
                    total_tokens: tokens.total_tokens(),
                    request_count: tokens.request_count,
                    cost,
                }
            })
            .collect::<Vec<_>>();
        let overview = overview(
            &dates,
            &daily,
            &hours,
            &calendar_days,
            today,
            favorite_model,
        );

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
                .iter()
                .map(|date| UsageDailyBucket {
                    date: date_key(*date),
                    tokens: daily.get(date).cloned().unwrap_or_default(),
                })
                .collect(),
            providers: provider_rows,
            models: model_rows,
            attribution,
            filters,
            collected_since,
            last_synced_at,
            cost,
            unpriced_tokens,
            overview,
            calendar,
            hourly,
            quota: self.quota_estimates(query_client, &price_rules, unix_time())?,
        })
    }

    fn cursor(&self, hash: &str) -> Result<Option<Cursor>> {
        self.db
            .connection
            .lock()
            .query_row(
                "SELECT modified_at,file_size,byte_offset,tail_fingerprint,parser_state FROM usage_sync_cursors WHERE path_hash=?1",
                [hash],
                |row| Ok(Cursor { modified_at: row.get(0)?, file_size: row.get(1)?, byte_offset: row.get(2)?, tail_fingerprint: row.get(3)?, parser_state: row.get(4)? }),
            )
            .optional()
            .map_err(Into::into)
    }

    fn save_cursor(&self, hash: &str, client: ClientKind, cursor: &Cursor, now: i64) -> Result<()> {
        self.db.connection.lock().execute(
            "INSERT INTO usage_sync_cursors(path_hash,client,modified_at,file_size,byte_offset,tail_fingerprint,updated_at,parser_state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(path_hash) DO UPDATE SET client=excluded.client,modified_at=excluded.modified_at,file_size=excluded.file_size,byte_offset=excluded.byte_offset,tail_fingerprint=excluded.tail_fingerprint,updated_at=excluded.updated_at,parser_state=excluded.parser_state",
            params![hash, client.to_string(), cursor.modified_at, cursor.file_size, cursor.byte_offset, cursor.tail_fingerprint, now, cursor.parser_state],
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

fn parse_codex_session(value: &Value, state: &mut CodexState) -> Option<UsageEvent> {
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        if let Some(source) = value
            .pointer("/payload/model_provider")
            .and_then(Value::as_str)
        {
            source.clone_into(&mut state.source);
        }
        state.account = value
            .pointer("/payload/creator_account_id")
            .and_then(Value::as_str)
            .filter(|account| !account.is_empty())
            .map(|account| digest(&format!("codex-account:{account}"))[..8].to_owned())
            .unwrap_or_default();
        return None;
    }
    let CodexState {
        model: current_model,
        cumulative,
        ..
    } = state;
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

/// The quota windows of a Codex `token_count` line. Older clients give `resets_in_seconds`
/// instead of an absolute `resets_at`.
fn codex_quota_readings(value: &Value, observed_at: i64) -> Vec<QuotaReading> {
    if value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
        return Vec::new();
    }
    let Some(limits) = value
        .pointer("/payload/rate_limits")
        .filter(|value| value.is_object())
    else {
        return Vec::new();
    };
    let limit_id = limits
        .get("limit_id")
        .and_then(Value::as_str)
        .unwrap_or("codex")
        .to_owned();
    let plan_type = limits
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|window| {
            let limit = limits.get(window).filter(|value| value.is_object())?;
            let used_percent = limit.get("used_percent").and_then(Value::as_f64)?;
            let window_minutes = limit.get("window_minutes").and_then(Value::as_i64)?;
            let resets_at = limit.get("resets_at").and_then(Value::as_i64).or_else(|| {
                limit
                    .get("resets_in_seconds")
                    .and_then(Value::as_i64)
                    .map(|seconds| observed_at + seconds)
            })?;
            (used_percent.is_finite()
                && (0.0..=100.0).contains(&used_percent)
                && window_minutes > 0)
                .then(|| QuotaReading {
                    window,
                    limit_id: limit_id.clone(),
                    window_minutes,
                    resets_at,
                    used_percent,
                    plan_type: plan_type.clone(),
                })
        })
        .collect()
}

struct ClaudeQuotaReading {
    account: String,
    plan_type: Option<String>,
    observed_at: i64,
    windows: Vec<ClaudeQuotaWindow>,
}

struct ClaudeQuotaWindow {
    window: &'static str,
    window_minutes: i64,
    resets_at: i64,
    used_percent: f64,
    share: f64,
}

/// The plan usage Claude Code cached in its global config: the weekly window, the account, and the
/// plan. The weekly window is shared with chat and other apps; its breakdown gives Claude Code's
/// share. The five-hour session window resets too often, and is refreshed too rarely, to estimate
/// an allowance from, so it is not read.
fn claude_quota_reading(config: &Value) -> Option<ClaudeQuotaReading> {
    let cached = config.get("cachedUsageUtilization")?;
    let observed_at = cached.get("fetchedAtMs").and_then(Value::as_i64)? / 1000;
    let account_id = cached
        .get("accountUuid")
        .or_else(|| config.pointer("/oauthAccount/accountUuid"))
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())?;
    let plan_type = [
        "organizationType",
        "userRateLimitTier",
        "organizationRateLimitTier",
    ]
    .iter()
    .find_map(|key| {
        config
            .pointer(&format!("/oauthAccount/{key}"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    })
    .map(str::to_owned);
    let utilization = cached.get("utilization")?;
    let share = utilization
        .pointer("/seven_day_breakdown/rows")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("key").and_then(Value::as_str) == Some("claude_code"))
        })
        .and_then(|row| row.get("percent"))
        .and_then(Value::as_f64)
        .map_or(1.0, |percent| (percent / 100.0).clamp(0.0, 1.0));
    let windows = [("seven_day", "secondary", 7 * 24 * 60, share)]
        .into_iter()
        .filter_map(|(name, window, window_minutes, share)| {
            let limit = utilization.get(name).filter(|value| value.is_object())?;
            let used_percent = limit.get("utilization").and_then(Value::as_f64)?;
            let resets_at = limit
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())?
                .timestamp();
            (used_percent.is_finite() && (0.0..=100.0).contains(&used_percent)).then_some(
                ClaudeQuotaWindow {
                    window,
                    window_minutes,
                    resets_at,
                    used_percent,
                    share,
                },
            )
        })
        .collect::<Vec<_>>();
    (!windows.is_empty()).then(|| ClaudeQuotaReading {
        account: digest(&format!("claude-account:{account_id}"))[..8].to_owned(),
        plan_type,
        observed_at,
        windows,
    })
}

/// Parses only the Codex lines the usage parser reads; the rest can be megabytes of transcript.
fn codex_line(bytes: &[u8]) -> Option<Value> {
    if bytes.len() > MAX_SESSION_LINE_BYTES {
        return None;
    }
    let relevant = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    if !relevant(b"turn_context") && !relevant(b"token_count") && !relevant(b"session_meta") {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// The Codex parser state after the complete lines before `offset`.
fn codex_state_at(path: &Path, offset: u64) -> Result<CodexState> {
    let mut state = CodexState::default();
    let mut reader = BufReader::new(File::open(path)?).take(offset);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        if let Some(value) = codex_line(&bytes) {
            parse_codex_session(&value, &mut state);
        }
    }
    Ok(state)
}

fn session_event_key(event: &UsageEvent, hash: &str, line_start: u64) -> String {
    let client = event.client;
    event.correlation_key.as_ref().map_or_else(
        || digest(&format!("session:{client}:{hash}:{line_start}")),
        |correlation| {
            digest(&format!(
                "session:{client}:{correlation}:{}",
                event.event_at
            ))
        },
    )
}

fn quota_reading_key(hash: &str, line_start: u64) -> String {
    digest(&format!("quota:{hash}:{line_start}"))
}

/// Tokens and cost spent while the meter moved, per cycle, pooled over the recent cycles:
/// allowance ≈ Σ tokens × 100 / Σ meter movement. Meter readings are whole percents, so each
/// cycle's movement is uncertain by one point either way, which the range reports.
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn estimate_quota(
    rows: &[QuotaRow],
    prices: &[ModelPrice],
    now: i64,
    today: NaiveDate,
) -> Option<UsageQuotaEstimate> {
    let latest = rows.last()?;
    // A cycle is every reading that names the same reset time. Concurrent sessions interleave
    // their readings, and one that started earlier can report a slightly older percentage, so a
    // dip inside a cycle is noise rather than a reset: a real reset names a new reset time.
    let mut cycles: Vec<Vec<&QuotaRow>> = Vec::new();
    for row in rows {
        match cycles.iter_mut().find(|cycle| {
            (cycle[0].resets_at - row.resets_at).abs() <= QUOTA_CYCLE_TOLERANCE_SECONDS
        }) {
            Some(cycle) => cycle.push(row),
            None => cycles.push(vec![row]),
        }
    }

    let mut resolved = HashMap::<&str, Option<&ModelPrice>>::new();
    let mut tokens = 0_u64;
    let mut cost = Vec::new();
    let mut movement = 0.0_f64;
    let mut used_cycles = 0_u32;
    let mut current_peak = latest.used_percent;
    let mut listed = Vec::new();
    for cycle in &cycles {
        // Movement is the rise of the running peak over the first reading. Usage only counts up
        // to the last rise: what follows it has not moved the meter yet.
        let first = cycle[0].used_percent;
        let mut peak = first;
        let mut settled = (0_u64, Vec::new());
        let mut pending = (0_u64, Vec::new());
        for row in &cycle[1..] {
            for (model, usage) in &row.usage {
                pending.0 += usage.total_tokens();
                let price = *resolved
                    .entry(model.as_str())
                    .or_insert_with(|| best_model_price(prices, None, model));
                if let Some(price) = price {
                    add_usage_cost(&mut pending.1, &price.currency, price.cost(usage));
                }
            }
            if row.used_percent > peak {
                peak = row.used_percent;
                settled.0 += pending.0;
                for entry in std::mem::take(&mut pending.1) {
                    add_usage_cost(&mut settled.1, &entry.currency, entry.amount);
                }
                pending.0 = 0;
            }
        }
        if (cycle[0].resets_at - latest.resets_at).abs() <= QUOTA_CYCLE_TOLERANCE_SECONDS {
            current_peak = peak;
        }
        listed.push(UsageQuotaCycle {
            resets_at: cycle[0].resets_at,
            first_at: cycle[0].observed_at,
            last_at: cycle
                .last()
                .map_or(cycle[0].observed_at, |row| row.observed_at),
            from_percent: first,
            to_percent: peak,
            tokens: settled.0,
            cost: settled.1.clone(),
        });
        // Only the client's share of the movement answers for the client's usage; a plan shared
        // with chat and other apps moves for them too.
        let share = cycle.last().map_or(1.0, |row| row.share).max(0.05);
        let moved = (peak - first) * share;
        if peak - first < 1.0 {
            continue;
        }
        movement += moved;
        used_cycles += 1;
        tokens += settled.0;
        for entry in settled.1 {
            add_usage_cost(&mut cost, &entry.currency, entry.amount);
        }
    }
    let used_percent = if now >= latest.resets_at {
        0.0
    } else {
        current_peak
    };
    let scale = |value: f64, percent: f64| value * 100.0 / percent;
    let capacity = (movement >= 1.0).then(|| {
        let slack = f64::from(used_cycles);
        UsageQuotaCapacity {
            tokens: token_count(scale(tokens as f64, movement)),
            tokens_low: token_count(scale(tokens as f64, movement + slack)),
            tokens_high: (movement > slack)
                .then(|| token_count(scale(tokens as f64, movement - slack))),
            cost: cost
                .iter()
                .map(|entry| UsageCost {
                    currency: entry.currency.clone(),
                    amount: scale(entry.amount, movement),
                })
                .collect(),
        }
    });
    let portion = |capacity: &UsageQuotaCapacity, factor: f64| UsageProjection {
        tokens: token_count(capacity.tokens as f64 * factor),
        cost: capacity
            .cost
            .iter()
            .map(|entry| UsageCost {
                currency: entry.currency.clone(),
                amount: entry.amount * factor,
            })
            .collect(),
    };
    let remaining = capacity
        .as_ref()
        .map(|capacity| portion(capacity, (100.0 - used_percent).max(0.0) / 100.0));
    let monthly = capacity
        .as_ref()
        .filter(|_| latest.window_minutes >= 24 * 60)
        .map(|capacity| {
            let minutes = f64::from(days_in_month(today)) * 24.0 * 60.0;
            portion(capacity, minutes / latest.window_minutes as f64)
        });
    listed.sort_by_key(|cycle| std::cmp::Reverse(cycle.first_at));
    listed.truncate(QUOTA_CYCLES_SHOWN);
    Some(UsageQuotaEstimate {
        plan_key: format!(
            "{}|{}|{}",
            latest.account,
            latest.source,
            latest.plan_type.as_deref().unwrap_or_default()
        ),
        account: (!latest.account.is_empty()).then(|| latest.account.clone()),
        source: (!latest.source.is_empty()).then(|| latest.source.clone()),
        current: true,
        limit_id: latest.limit_id.clone(),
        window: latest.window.clone(),
        window_minutes: u32::try_from(latest.window_minutes).unwrap_or(u32::MAX),
        plan_type: latest.plan_type.clone(),
        used_percent,
        resets_at: latest.resets_at,
        observed_at: latest.observed_at,
        basis_percent: movement,
        basis_cycles: used_cycles,
        capacity,
        remaining,
        monthly,
        cycles: listed,
    })
}

fn days_in_month(date: NaiveDate) -> u32 {
    let next = if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
    };
    next.and_then(|next| next.pred_opt())
        .map_or(30, |last| last.day())
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

fn filter_options(rows: &[RollupRow]) -> UsageFilterOptions {
    let mut providers = HashMap::<String, UsageProviderOption>::new();
    let mut models = BTreeSet::<String>::new();
    for row in rows {
        let inferred = row.attribution[1] > 0;
        if let Some(id) = &row.provider_id {
            providers
                .entry(id.clone())
                .and_modify(|option| option.inferred |= inferred)
                .or_insert_with(|| UsageProviderOption {
                    id: id.clone(),
                    name: row.provider_name.clone(),
                    inferred,
                });
        }
        models.insert(row.model.clone());
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

/// Every date from `from` to `to`, both included.
fn date_range(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    from.iter_days().take_while(|date| *date <= to).collect()
}

fn date_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn local_naive_date(timestamp: i64) -> Option<NaiveDate> {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|value| value.with_timezone(&Local).date_naive())
}

fn local_date(timestamp: i64) -> String {
    local_naive_date(timestamp).map_or_else(|| "unknown".into(), date_key)
}

fn local_hour(timestamp: i64) -> u8 {
    DateTime::<Utc>::from_timestamp(timestamp, 0).map_or(0, |value| {
        u8::try_from(value.with_timezone(&Local).hour()).unwrap_or(0)
    })
}

fn local_midnight(date: NaiveDate) -> Option<i64> {
    date.and_hms_opt(0, 0, 0)
        .and_then(|value| value.and_local_timezone(Local).earliest())
        .map(|value| value.timestamp())
}

/// The `[start, end)` timestamps of a local calendar day.
fn local_day_bounds(day: &str) -> Option<(i64, i64)> {
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    Some((local_midnight(date)?, local_midnight(date.succ_opt()?)?))
}

/// Highlights of the queried days; the current streak looks back from today through the
/// calendar instead, so a range that ended earlier does not freeze it.
fn overview(
    dates: &[NaiveDate],
    daily: &BTreeMap<NaiveDate, UsageTokenSummary>,
    hours: &[u64; 24],
    calendar: &BTreeMap<NaiveDate, (UsageTokenSummary, Vec<UsageCost>)>,
    today: NaiveDate,
    favorite_model: Option<String>,
) -> UsageOverview {
    let active = |tokens: Option<&UsageTokenSummary>| {
        tokens.is_some_and(|tokens| tokens.request_count > 0 || tokens.total_tokens() > 0)
    };
    let mut longest_streak = 0;
    let mut run = 0;
    for date in dates {
        if active(daily.get(date)) {
            run += 1;
            longest_streak = longest_streak.max(run);
        } else {
            run = 0;
        }
    }
    let calendar_active = |date: &NaiveDate| active(calendar.get(date).map(|(tokens, _)| tokens));
    let mut day = if calendar_active(&today) {
        Some(today)
    } else {
        today.pred_opt()
    };
    let mut current_streak = 0;
    while let Some(date) = day.filter(calendar_active) {
        current_streak += 1;
        day = date.pred_opt();
    }
    let most_active_day = daily
        .iter()
        .filter(|(_, tokens)| tokens.total_tokens() > 0)
        .max_by_key(|(date, tokens)| (tokens.total_tokens(), std::cmp::Reverse(**date)))
        .map(|(date, tokens)| UsageDailyBucket {
            date: date_key(*date),
            tokens: tokens.clone(),
        });
    let peak_hour = (0..24_u8)
        .filter(|hour| hours[usize::from(*hour)] > 0)
        .max_by_key(|hour| (hours[usize::from(*hour)], std::cmp::Reverse(*hour)));
    UsageOverview {
        favorite_model,
        active_days: daily.values().filter(|tokens| active(Some(tokens))).count() as u64,
        current_streak,
        longest_streak,
        most_active_day,
        peak_hour,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn token_count(value: f64) -> u64 {
    // Projections are non-negative sums of token counts, far inside u64.
    value.max(0.0).round() as u64
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

    fn tokens(input: u64, output: u64) -> UsageTokenSummary {
        UsageTokenSummary {
            input_tokens: input,
            output_tokens: output,
            request_count: 1,
            ..UsageTokenSummary::default()
        }
    }

    fn proxy_event(key: &str, model: &str, event_at: i64, tokens: UsageTokenSummary) -> UsageEvent {
        UsageEvent {
            client: ClientKind::Codex,
            source: UsageDataSource::Proxy,
            provider_id: Some("provider-1".into()),
            provider_name: "Provider".into(),
            provider_revision: 1,
            model: model.into(),
            event_at,
            tokens,
            attribution: UsageAttribution::Exact,
            dedup_key: key.into(),
            correlation_key: None,
        }
    }

    fn all_time(client: ClientKind) -> UsageStatsQuery {
        UsageStatsQuery {
            client,
            from: 0,
            to: unix_time() + 60,
            provider_id: None,
            model: None,
        }
    }

    #[test]
    fn rollups_match_raw_events_and_survive_raw_cleanup() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let now = unix_time();
        collector
            .insert_event(&proxy_event("a", "gpt-5.5", now, tokens(1_000, 10)))
            .expect("first");
        collector
            .insert_event(&proxy_event("b", "gpt-5.5", now, tokens(3_000, 30)))
            .expect("second");
        let before = collector.query(all_time(ClientKind::Codex)).expect("query");
        assert_eq!(before.summary.request_count, 2);
        assert_eq!(before.summary.total_tokens(), 4_040);
        assert_eq!(before.attribution.exact, 2);

        db.connection
            .lock()
            .execute("DELETE FROM usage_events", [])
            .expect("simulate expiry");
        let after = collector.query(all_time(ClientKind::Codex)).expect("query");
        assert_eq!(after.summary, before.summary);
        assert_eq!(after.overview.current_streak, 1);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn all_time_reaches_past_the_raw_retention_window() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let old = Local::now().date_naive() - chrono::Duration::days(200);
        db.connection
            .lock()
            .execute(
                "INSERT INTO usage_hourly(client,day,hour,provider_key,provider_id,provider_name,provider_revision,model,input_tokens,output_tokens,request_count,exact_count) VALUES('codex',?1,9,'p','p','Provider',1,'gpt-5.5',500,5,1,1)",
                [date_key(old)],
            )
            .expect("aged rollup");
        let report = collector.query(all_time(ClientKind::Codex)).expect("query");
        assert!(report.daily.len() > 92, "{} days", report.daily.len());
        assert_eq!(
            report.daily.first().map(|day| day.date.clone()),
            Some(date_key(old))
        );
        assert_eq!(report.summary.total_tokens(), 505);
        assert_eq!(report.overview.peak_hour, Some(9));
        assert_eq!(report.overview.active_days, 1);
        assert_eq!(report.calendar.len(), USAGE_CALENDAR_DAYS);
        assert!(report.models[0].daily.len() <= USAGE_MODEL_SERIES_DAYS);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cost_is_summed_per_currency_and_unpriced_tokens_are_counted() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        crate::pricing::set_user_price(
            &db,
            &hsin_core::ModelPriceInput {
                id: None,
                model_pattern: "local-*".into(),
                provider_id: None,
                currency: "CNY".into(),
                input: 2.0,
                cache_write: None,
                cache_read: None,
                output: 8.0,
            },
            0,
        )
        .expect("user price");
        let now = unix_time();
        for (key, model) in [("a", "gpt-5.5"), ("b", "local-model"), ("c", "mystery")] {
            collector
                .insert_event(&proxy_event(key, model, now, tokens(1_000_000, 100_000)))
                .expect("event");
        }
        let report = collector.query(all_time(ClientKind::Codex)).expect("query");
        let amount = |currency: &str| {
            report
                .cost
                .iter()
                .find(|cost| cost.currency == currency)
                .map(|cost| cost.amount)
        };
        assert!((amount("USD").unwrap() - (5.0 + 3.0)).abs() < 1e-9);
        assert!((amount("CNY").unwrap() - (2.0 + 0.8)).abs() < 1e-9);
        assert_eq!(report.unpriced_tokens, 1_100_000);
        let local = report
            .models
            .iter()
            .find(|model| model.model == "local-model")
            .expect("local model");
        assert_eq!(local.cost.len(), 1);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn overview_tracks_streaks_and_the_busiest_day() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let dates = date_range(today - chrono::Duration::days(6), today);
        let daily = [0, 1, 2, 4, 5]
            .into_iter()
            .map(|index| (dates[index], tokens(10 * (index as u64 + 1), 0)))
            .collect::<BTreeMap<_, _>>();
        let calendar = daily
            .iter()
            .map(|(date, tokens)| (*date, (tokens.clone(), Vec::new())))
            .collect::<BTreeMap<_, _>>();
        let mut hours = [0_u64; 24];
        hours[14] = 9;
        hours[3] = 9;
        let overview = overview(&dates, &daily, &hours, &calendar, today, None);
        assert_eq!(overview.active_days, 5);
        assert_eq!(overview.longest_streak, 3);
        // Today (index 6) is idle, so the streak runs back from yesterday.
        assert_eq!(overview.current_streak, 2);
        assert_eq!(overview.most_active_day.unwrap().date, date_key(dates[5]));
        assert_eq!(overview.peak_hour, Some(3));
    }

    fn codex_turn(model: &str) -> String {
        serde_json::json!({"type":"turn_context","timestamp":Utc::now().to_rfc3339(),"payload":{"model":model}})
            .to_string()
    }

    fn codex_usage(input: u64, used_percent: Option<f64>, resets_at: i64) -> String {
        let mut payload = serde_json::json!({
            "type": "token_count",
            "info": {"last_token_usage": {"input_tokens": input, "output_tokens": 0}}
        });
        if let Some(used_percent) = used_percent {
            payload["rate_limits"] = serde_json::json!({
                "limit_id": "codex",
                "plan_type": "pro",
                "primary": {"used_percent": used_percent, "window_minutes": 10080, "resets_at": resets_at},
                "secondary": null
            });
        }
        serde_json::json!({"type":"event_msg","timestamp":Utc::now().to_rfc3339(),"payload":payload})
            .to_string()
    }

    fn codex_models(collector: &UsageCollector) -> Vec<(String, u64)> {
        collector
            .query(all_time(ClientKind::Codex))
            .expect("query")
            .models
            .into_iter()
            .map(|model| (model.model, model.tokens.request_count))
            .collect()
    }

    #[test]
    fn codex_model_survives_a_sync_that_resumes_mid_file() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let log = root.join("codex/sessions/resumed.jsonl");
        fs::write(
            &log,
            format!(
                "{}\n{}\n",
                codex_turn("gpt-6.1-sol"),
                codex_usage(10, None, 0)
            ),
        )
        .expect("first turn");
        collector.sync().expect("first sync");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("append");
        writeln!(file, "{}", codex_usage(20, None, 0)).expect("second usage");
        drop(file);
        collector.sync().expect("resumed sync");
        assert_eq!(codex_models(&collector), [("gpt-6.1-sol".to_owned(), 2)]);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn repair_relabels_events_an_earlier_sync_filed_under_unknown() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let log = root.join("codex/sessions/old.jsonl");
        let turn = codex_turn("gpt-6-astra");
        let usage = codex_usage(30, None, 0);
        fs::write(&log, format!("{turn}\n{usage}\n")).expect("log");
        // What a resumed sync used to store: the same line, under `unknown`.
        let mut parser = CodexState::default();
        let mut stale = parse_codex_session(&serde_json::from_str(&usage).unwrap(), &mut parser)
            .expect("usage event");
        let line_start = turn.len() as u64 + 1;
        stale.dedup_key = session_event_key(&stale, &path_hash(&log), line_start);
        collector.insert_event(&stale).expect("stale event");
        assert_eq!(codex_models(&collector), [(UNKNOWN_MODEL.to_owned(), 1)]);

        db.connection
            .lock()
            .execute("DELETE FROM usage_meta WHERE key=?1", [CODEX_REPAIR_KEY])
            .expect("rearm repair");
        collector
            .repair_codex_sessions(unix_time())
            .expect("repair");
        assert_eq!(codex_models(&collector), [("gpt-6-astra".to_owned(), 1)]);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn quota_readings_back_calculate_the_weekly_allowance() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let resets_at = unix_time() + 3 * 86_400;
        fs::write(
            root.join("codex/sessions/quota.jsonl"),
            [
                codex_turn("gpt-6.1-sol"),
                codex_usage(1_000_000, Some(10.0), resets_at),
                codex_usage(500_000, Some(11.0), resets_at),
                codex_usage(500_000, Some(12.0), resets_at),
            ]
            .map(|line| line + "\n")
            .concat(),
        )
        .expect("log");
        collector.sync().expect("sync");
        let report = collector.query(all_time(ClientKind::Codex)).expect("query");
        let [quota] = &report.quota[..] else {
            panic!("one weekly window: {:?}", report.quota);
        };
        assert_eq!(quota.plan_type.as_deref(), Some("pro"));
        assert_eq!(quota.window_minutes, 10_080);
        assert!((quota.used_percent - 12.0).abs() < f64::EPSILON);
        let capacity = quota.capacity.as_ref().expect("capacity");
        // Two points of movement bought one million tokens.
        assert_eq!(capacity.tokens, 50_000_000);
        assert_eq!(capacity.tokens_low, 33_333_333);
        assert_eq!(capacity.tokens_high, Some(100_000_000));
        // One million GPT-6.1 Sol input tokens list at $2.
        assert!((capacity.cost[0].amount - 100.0).abs() < 1e-6);
        assert_eq!(quota.remaining.as_ref().unwrap().tokens, 44_000_000);
        let days = f64::from(days_in_month(Local::now().date_naive()));
        assert_eq!(
            quota.monthly.as_ref().unwrap().tokens,
            token_count(50_000_000.0 * days / 7.0)
        );
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn quota_readings_accept_relative_resets_and_both_windows() {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {"used_percent": 40.0, "window_minutes": 300, "resets_in_seconds": 600},
                    "secondary": {"used_percent": 5.0, "window_minutes": 10080, "resets_at": 99},
                    "plan_type": "plus"
                }
            }
        });
        let readings = codex_quota_readings(&value, 1_000);
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].resets_at, 1_600);
        assert_eq!(readings[0].limit_id, "codex");
        assert_eq!(readings[1].window, "secondary");
        assert_eq!(readings[1].plan_type.as_deref(), Some("plus"));
        let unmetered = serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":null}}});
        assert_eq!(codex_quota_readings(&unmetered, 0).len(), 0);
    }

    #[test]
    fn a_meter_that_barely_moved_gives_no_estimate() {
        let row = |used_percent: f64, input: u64| QuotaRow {
            account: String::new(),
            source: "openai".into(),
            window: "primary".into(),
            limit_id: "codex".into(),
            window_minutes: 300,
            resets_at: 10_000,
            used_percent,
            plan_type: None,
            observed_at: 0,
            usage: vec![("gpt-6.1-sol".into(), tokens(input, 0))],
            share: 1.0,
        };
        let today = Local::now().date_naive();
        let flat = estimate_quota(&[row(3.0, 0), row(3.0, 500)], &[], 0, today).unwrap();
        assert!(flat.capacity.is_none());
        // A reset names a new reset time and starts a new cycle.
        let next = |used_percent: f64, input: u64| QuotaRow {
            resets_at: 30_000,
            ..row(used_percent, input)
        };
        let reset = estimate_quota(
            &[row(90.0, 0), row(92.0, 100), next(1.0, 0), next(3.0, 100)],
            &[],
            0,
            today,
        )
        .unwrap();
        assert!((reset.basis_percent - 4.0).abs() < f64::EPSILON);
        assert_eq!(reset.basis_cycles, 2);
        assert!(
            reset.monthly.is_none(),
            "a five-hour window has no monthly figure"
        );
        assert_eq!(reset.capacity.unwrap().tokens, 5_000);
    }

    #[test]
    fn stale_readings_from_concurrent_sessions_are_not_resets() {
        let row = |used_percent: f64, input: u64| QuotaRow {
            account: String::new(),
            source: "openai".into(),
            window: "primary".into(),
            limit_id: "codex".into(),
            window_minutes: 10_080,
            resets_at: 10_000,
            used_percent,
            plan_type: Some("pro".into()),
            observed_at: 0,
            usage: vec![("gpt-6.1-sol".into(), tokens(input, 0))],
            share: 1.0,
        };
        // A session still reporting 4% interleaves with one at 5%; the trailing 900 tokens have
        // not moved the meter yet and stay out.
        let estimate = estimate_quota(
            &[
                row(4.0, 0),
                row(5.0, 100),
                row(4.0, 100),
                row(6.0, 100),
                row(6.0, 900),
            ],
            &[],
            0,
            Local::now().date_naive(),
        )
        .unwrap();
        assert_eq!(estimate.basis_cycles, 1);
        assert!((estimate.basis_percent - 2.0).abs() < f64::EPSILON);
        assert!((estimate.used_percent - 6.0).abs() < f64::EPSILON);
        assert_eq!(estimate.capacity.unwrap().tokens, 15_000);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn plans_accounts_and_providers_are_estimated_apart() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let now = unix_time();
        let at = |seconds_ago: i64| {
            DateTime::<Utc>::from_timestamp(now - seconds_ago, 0)
                .unwrap()
                .to_rfc3339()
        };
        let meta = |provider: &str| {
            serde_json::json!({"type":"session_meta","timestamp":at(9_000),"payload":{"model_provider":provider,"creator_account_id":"account-1"}})
                .to_string()
        };
        let usage = |seconds_ago: i64, input: u64, limits: Option<(&str, f64, i64)>| {
            let mut payload = serde_json::json!({
                "type": "token_count",
                "info": {"last_token_usage": {"input_tokens": input, "output_tokens": 0}}
            });
            if let Some((plan, used_percent, resets_at)) = limits {
                payload["rate_limits"] = serde_json::json!({
                    "limit_id": "codex",
                    "plan_type": plan,
                    "primary": {"used_percent": used_percent, "window_minutes": 10080, "resets_at": resets_at}
                });
            }
            serde_json::json!({"type":"event_msg","timestamp":at(seconds_ago),"payload":payload})
                .to_string()
        };
        let write = |name: &str, lines: Vec<String>| {
            fs::write(
                root.join("codex/sessions").join(name),
                lines
                    .into_iter()
                    .map(|line| line + "\n")
                    .collect::<String>(),
            )
            .expect("log");
        };
        let lite = now + 86_400;
        let pro = now + 6 * 86_400;
        write(
            "lite.jsonl",
            vec![
                meta("openai"),
                codex_turn("gpt-6.1-sol"),
                usage(7_300, 100_000, Some(("prolite", 10.0, lite))),
                usage(7_250, 100_000, Some(("prolite", 11.0, lite))),
                usage(7_200, 100_000, Some(("prolite", 12.0, lite))),
            ],
        );
        write(
            "pro.jsonl",
            vec![
                meta("openai"),
                codex_turn("gpt-6.1-sol"),
                usage(20, 1_000_000, Some(("pro", 1.0, pro))),
                usage(10, 1_000_000, Some(("pro", 2.0, pro))),
            ],
        );
        // An older client did not name the account; with one known Pro Lite account, it is that one.
        write(
            "lite-unnamed.jsonl",
            vec![
                serde_json::json!({"type":"session_meta","timestamp":at(9_000),"payload":{"model_provider":"openai"}})
                    .to_string(),
                codex_turn("gpt-6.1-sol"),
                usage(8_000, 0, Some(("prolite", 50.0, lite - 7 * 86_400))),
                usage(7_900, 300_000, Some(("prolite", 53.0, lite - 7 * 86_400))),
            ],
        );
        // A relay session at the same time reports no quota and must not dilute either plan.
        write(
            "relay.jsonl",
            vec![
                meta("hsin"),
                codex_turn("gpt-6.1-sol"),
                usage(15, 5_000_000, None),
            ],
        );
        collector.sync().expect("sync");
        let quota = collector
            .query(all_time(ClientKind::Codex))
            .expect("query")
            .quota;
        assert_eq!(quota.len(), 2, "{quota:?}");
        let (current, past) = (&quota[0], &quota[1]);
        assert_eq!(current.plan_type.as_deref(), Some("pro"));
        assert!(current.current);
        assert_eq!(current.source.as_deref(), Some("openai"));
        assert_eq!(current.account.as_ref().map(String::len), Some(8));
        assert_eq!(current.capacity.as_ref().unwrap().tokens, 100_000_000);
        assert!(current.remaining.is_some());
        assert_eq!(past.plan_type.as_deref(), Some("prolite"));
        assert!(!past.current);
        assert!(past.remaining.is_none());
        assert_eq!(past.account, current.account);
        // 200k over two points and 300k over three pool to 500k over five.
        assert_eq!(past.basis_cycles, 2);
        assert_eq!(past.capacity.as_ref().unwrap().tokens, 10_000_000);
        assert_eq!(past.cycles.len(), 2);
        assert_eq!(past.cycles[0].tokens, 200_000);
        assert!((past.cycles[0].to_percent - 12.0).abs() < f64::EPSILON);
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }

    fn claude_config(fetched_at: i64, weekly: f64, resets_at: i64) -> serde_json::Value {
        let resets = DateTime::<Utc>::from_timestamp(resets_at, 0)
            .unwrap()
            .to_rfc3339();
        serde_json::json!({
            "oauthAccount": {"accountUuid": "account-uuid", "emailAddress": "someone@example.test", "organizationType": "claude_pro"},
            "cachedUsageUtilization": {
                "fetchedAtMs": fetched_at * 1000,
                "accountUuid": "account-uuid",
                "utilization": {
                    "five_hour": {"utilization": 21, "resets_at": resets},
                    "seven_day": {"utilization": weekly, "resets_at": resets},
                    "seven_day_opus": null,
                    "seven_day_breakdown": {"rows": [
                        {"key": "claude_code", "percent": 80},
                        {"key": "chat", "percent": 20}
                    ]}
                }
            }
        })
    }

    #[test]
    fn claude_readings_come_from_the_cached_plan_usage_only() {
        let reading = claude_quota_reading(&claude_config(1_000, 49.0, 5_000)).expect("reading");
        assert_eq!(reading.observed_at, 1_000);
        assert_eq!(reading.plan_type.as_deref(), Some("claude_pro"));
        assert_eq!(reading.account.len(), 8);
        assert!(!reading.account.contains("account"));
        let [weekly] = &reading.windows[..] else {
            panic!("only the weekly window is read");
        };
        assert_eq!(weekly.window_minutes, 10_080);
        assert!((weekly.share - 0.8).abs() < f64::EPSILON);
        assert!((weekly.used_percent - 49.0).abs() < f64::EPSILON);
        assert!(claude_quota_reading(&serde_json::json!({"oauthAccount": {}})).is_none());
    }

    #[test]
    fn claude_quota_credits_official_usage_between_readings() {
        let (root, db, collector) = test_collector();
        collector.initialize().expect("initialize");
        let now = unix_time();
        let resets_at = now + 3 * 86_400;
        let config = root.join("claude/.claude.json");
        fs::write(
            &config,
            claude_config(now - 1_000, 10.0, resets_at).to_string(),
        )
        .expect("first");
        collector.sync().expect("first reading");
        let event = |key: &str, provider: &str, input: u64| UsageEvent {
            client: ClientKind::Claude,
            source: UsageDataSource::Session,
            provider_id: Some(provider.into()),
            provider_name: provider.into(),
            provider_revision: 1,
            model: "claude-sonnet-5".into(),
            event_at: now - 700,
            tokens: tokens(input, 0),
            attribution: UsageAttribution::Inferred,
            dedup_key: key.into(),
            correlation_key: None,
        };
        collector
            .insert_event(&event("plan", "official-claude", 800_000))
            .expect("plan usage");
        // A relay request in the same window is not the plan's.
        collector
            .insert_event(&event("relay", "relay-provider", 9_000_000))
            .expect("relay usage");
        fs::write(
            &config,
            claude_config(now - 500, 12.0, resets_at).to_string(),
        )
        .expect("second");
        *collector.claude_config_seen.lock() = None;
        collector.sync().expect("second reading");

        let quota = collector
            .query(all_time(ClientKind::Claude))
            .expect("query")
            .quota;
        let weekly = quota
            .iter()
            .find(|estimate| estimate.window_minutes == 10_080)
            .expect("weekly window");
        assert_eq!(weekly.plan_type.as_deref(), Some("claude_pro"));
        assert_eq!(weekly.source.as_deref(), Some("anthropic"));
        // 800k tokens moved the meter two points, of which Claude Code caused 80%.
        assert!((weekly.basis_percent - 1.6).abs() < 1e-9);
        assert_eq!(weekly.capacity.as_ref().unwrap().tokens, 50_000_000);
        // Sonnet 5 input lists at $2 per million.
        assert!((weekly.capacity.as_ref().unwrap().cost[0].amount - 100.0).abs() < 1e-6);
        assert_eq!(quota.len(), 1, "no five-hour window: {quota:?}");
        drop(collector);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }
}
