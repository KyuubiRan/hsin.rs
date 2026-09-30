//! Model prices used to estimate the cost of recorded usage.
//!
//! Three layers feed one list: prices shipped with the release, prices fetched on request from the
//! public `LiteLLM` price table, and rules the user enters. Only the last two are stored; the
//! built-in table lives in the binary so an upgrade refreshes it.

use std::time::Duration;

use futures_util::StreamExt;
use hsin_core::{
    ModelPrice, ModelPriceInput, ModelPriceList, ModelPriceSource, PricingRefreshResult,
    normalize_model_name,
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::{
    db::Database,
    error::{DaemonError, Result},
    network_proxy::{ClientOptions, OutboundProxySnapshot, build_client},
};

pub(crate) const REMOTE_PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MAX_REMOTE_BYTES: usize = 8 * 1024 * 1024;
const REMOTE_FETCHED_AT_KEY: &str = "pricing_remote_fetched_at";
/// Vendors whose first-party prices are imported. Gateways and clouds re-list the same models at
/// their own prices under other prefixes, which would shadow the vendor's own rate.
const REMOTE_PROVIDERS: [&str; 6] = [
    "openai",
    "anthropic",
    "deepseek",
    "gemini",
    "xai",
    "mistral",
];

/// `(pattern, currency, input, cache write, cache read, output)`, per million tokens.
type BuiltinPrice = (
    &'static str,
    &'static str,
    f64,
    Option<f64>,
    Option<f64>,
    f64,
);

/// When the built-in table was last checked against the vendors' price pages.
const BUILTIN_AS_OF: i64 = 1_790_726_400; // 2026-09-30T00:00:00Z

/// Standard first-party list prices. `DeepSeek` bills half these rates off-peak; the peak rate is
/// kept so estimates err high.
const BUILTIN_PRICES: &[BuiltinPrice] = &[
    (
        "claude-fable-5-1",
        "USD",
        10.0,
        Some(12.5),
        Some(0.25),
        50.0,
    ),
    ("claude-fable-5", "USD", 10.0, Some(12.5), Some(1.0), 50.0),
    (
        "claude-mythos-5-1",
        "USD",
        10.0,
        Some(12.5),
        Some(1.0),
        50.0,
    ),
    ("claude-opus-5-5", "USD", 4.0, Some(5.0), Some(0.2), 20.0),
    ("claude-opus-5", "USD", 5.0, Some(6.25), Some(0.5), 25.0),
    ("claude-opus-4-8", "USD", 5.0, Some(6.25), Some(0.5), 25.0),
    ("claude-opus-4-7", "USD", 5.0, Some(6.25), Some(0.5), 25.0),
    ("claude-opus-4-6", "USD", 5.0, Some(6.25), Some(0.5), 25.0),
    ("claude-opus-4-5", "USD", 5.0, Some(6.25), Some(0.5), 25.0),
    ("claude-opus-4*", "USD", 15.0, Some(18.75), Some(1.5), 75.0),
    ("claude-sonnet-5", "USD", 2.0, Some(2.5), Some(0.2), 10.0),
    ("claude-sonnet-4*", "USD", 3.0, Some(3.75), Some(0.3), 15.0),
    ("claude-haiku-4-5", "USD", 1.0, Some(1.25), Some(0.1), 5.0),
    ("deepseek-v4-pro*", "USD", 1.32, None, Some(0.044), 3.96),
    ("deepseek-v4-flash*", "USD", 0.3, None, Some(0.006), 1.2),
    ("deepseek-flash*", "USD", 0.3, None, Some(0.006), 1.2),
    ("gpt-5.6-sol*", "USD", 4.0, None, Some(0.4), 20.0),
    ("gpt-5.6-terra*", "USD", 2.0, None, Some(0.2), 12.0),
    ("gpt-5.6-luna*", "USD", 0.2, None, Some(0.02), 1.2),
    ("gpt-5.5*", "USD", 5.0, None, Some(0.5), 30.0),
    ("gpt-5.4-pro*", "USD", 30.0, None, None, 180.0),
    ("gpt-5.4-nano*", "USD", 0.1, None, Some(0.01), 0.63),
    ("gpt-5.4*", "USD", 2.5, None, Some(0.25), 15.0),
];

pub(crate) fn builtin_prices() -> Vec<ModelPrice> {
    BUILTIN_PRICES
        .iter()
        .map(
            |&(pattern, currency, input, cache_write, cache_read, output)| ModelPrice {
                id: format!("builtin:{pattern}"),
                model_pattern: pattern.into(),
                provider_id: None,
                currency: currency.into(),
                input,
                cache_write,
                cache_read,
                output,
                source: ModelPriceSource::Builtin,
                updated_at: BUILTIN_AS_OF,
            },
        )
        .collect()
}

/// Every price rule, built-in ones first.
pub(crate) fn all_prices(db: &Database) -> Result<Vec<ModelPrice>> {
    let mut prices = builtin_prices();
    prices.extend(stored_prices(db)?);
    Ok(prices)
}

pub(crate) fn list(db: &Database) -> Result<ModelPriceList> {
    let remote_fetched_at = db
        .connection
        .lock()
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [REMOTE_FETCHED_AT_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse().ok());
    Ok(ModelPriceList {
        prices: all_prices(db)?,
        remote_fetched_at,
    })
}

fn stored_prices(db: &Database) -> Result<Vec<ModelPrice>> {
    let connection = db.connection.lock();
    let mut statement = connection.prepare(
        "SELECT id,model_pattern,provider_id,currency,input_price,cache_write_price,cache_read_price,output_price,source,updated_at FROM model_prices ORDER BY source,model_pattern",
    )?;
    let rows = statement.query_map([], |row| {
        let source: String = row.get(8)?;
        Ok(ModelPrice {
            id: row.get(0)?,
            model_pattern: row.get(1)?,
            provider_id: row.get(2)?,
            currency: row.get(3)?,
            input: row.get(4)?,
            cache_write: row.get(5)?,
            cache_read: row.get(6)?,
            output: row.get(7)?,
            source: if source == "user" {
                ModelPriceSource::User
            } else {
                ModelPriceSource::Remote
            },
            updated_at: row.get(9)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Adds a user rule, or replaces the user rule `input.id` names.
pub(crate) fn set_user_price(
    db: &Database,
    input: &ModelPriceInput,
    now: i64,
) -> Result<ModelPrice> {
    input
        .validate()
        .map_err(|error| DaemonError::Invalid(error.to_string()))?;
    let connection = db.connection.lock();
    let id = match &input.id {
        Some(id) => {
            let source: Option<String> = connection
                .query_row("SELECT source FROM model_prices WHERE id=?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            match source.as_deref() {
                Some("user") => id.clone(),
                Some(_) => {
                    return Err(DaemonError::Invalid(
                        "only user price rules can be edited".into(),
                    ));
                }
                None => return Err(DaemonError::NotFound(format!("price rule {id}"))),
            }
        }
        None => format!("user:{}", uuid::Uuid::new_v4()),
    };
    let price = ModelPrice {
        id,
        model_pattern: input.model_pattern.clone(),
        provider_id: input.provider_id.clone(),
        currency: input.currency.clone(),
        input: input.input,
        cache_write: input.cache_write,
        cache_read: input.cache_read,
        output: input.output,
        source: ModelPriceSource::User,
        updated_at: now,
    };
    connection.execute(
        "INSERT INTO model_prices(id,model_pattern,provider_id,currency,input_price,cache_write_price,cache_read_price,output_price,source,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'user',?9) ON CONFLICT(id) DO UPDATE SET model_pattern=excluded.model_pattern,provider_id=excluded.provider_id,currency=excluded.currency,input_price=excluded.input_price,cache_write_price=excluded.cache_write_price,cache_read_price=excluded.cache_read_price,output_price=excluded.output_price,updated_at=excluded.updated_at",
        params![
            price.id,
            price.model_pattern,
            price.provider_id,
            price.currency,
            price.input,
            price.cache_write,
            price.cache_read,
            price.output,
            price.updated_at,
        ],
    )?;
    Ok(price)
}

pub(crate) fn remove_user_price(db: &Database, id: &str) -> Result<()> {
    let removed = db.connection.lock().execute(
        "DELETE FROM model_prices WHERE id=?1 AND source='user'",
        [id],
    )?;
    if removed == 0 {
        return Err(DaemonError::NotFound(format!("user price rule {id}")));
    }
    Ok(())
}

/// Downloads the public price table through the configured upstream proxy and replaces every
/// fetched rule in one transaction. User rules are never touched.
pub(crate) async fn refresh(
    db: &Database,
    proxy: &OutboundProxySnapshot,
    now: i64,
) -> Result<PricingRefreshResult> {
    let client = build_client(
        proxy,
        ClientOptions {
            connect_timeout: Duration::from_secs(10),
            timeout: Some(Duration::from_secs(60)),
        },
    )
    .await?;
    let response = client
        .get(REMOTE_PRICES_URL)
        .send()
        .await
        .map_err(|error| DaemonError::Config(format!("price list request failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DaemonError::Config(format!(
            "price list returned HTTP {status}"
        )));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| DaemonError::Config(format!("price list download failed: {error}")))?;
        if body.len().saturating_add(chunk.len()) > MAX_REMOTE_BYTES {
            return Err(DaemonError::Config("price list is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    let prices = parse_remote(&body, now)?;
    store_remote(db, &prices, now)?;
    Ok(PricingRefreshResult {
        imported: u64::try_from(prices.len()).unwrap_or(u64::MAX),
        fetched_at: now,
    })
}

fn store_remote(db: &Database, prices: &[ModelPrice], now: i64) -> Result<()> {
    let mut connection = db.connection.lock();
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM model_prices WHERE source='remote'", [])?;
    for price in prices {
        transaction.execute(
            "INSERT INTO model_prices(id,model_pattern,provider_id,currency,input_price,cache_write_price,cache_read_price,output_price,source,updated_at) VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,'remote',?8)",
            params![
                price.id,
                price.model_pattern,
                price.currency,
                price.input,
                price.cache_write,
                price.cache_read,
                price.output,
                now,
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
        params![REMOTE_FETCHED_AT_KEY, now.to_string(), now],
    )?;
    transaction.commit()?;
    Ok(())
}

/// Extracts chat-model prices from the `LiteLLM` table. Prices there are USD per token.
pub(crate) fn parse_remote(body: &[u8], now: i64) -> Result<Vec<ModelPrice>> {
    if body.len() > MAX_REMOTE_BYTES {
        return Err(DaemonError::Config("price list is too large".into()));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| DaemonError::Config(format!("invalid price list: {error}")))?;
    let Some(entries) = value.as_object() else {
        return Err(DaemonError::Config(
            "invalid price list: not an object".into(),
        ));
    };
    let per_million = |entry: &Value, key: &str| {
        entry
            .get(key)
            .and_then(Value::as_f64)
            .map(|price| price * 1_000_000.0)
            .filter(|price| price.is_finite() && (0.0..=1_000_000.0).contains(price))
    };
    let mut prices = std::collections::BTreeMap::<String, ModelPrice>::new();
    for (name, entry) in entries {
        let provider = entry.get("litellm_provider").and_then(Value::as_str);
        let mode = entry.get("mode").and_then(Value::as_str);
        if !provider.is_some_and(|provider| REMOTE_PROVIDERS.contains(&provider))
            || !matches!(mode, Some("chat" | "responses"))
        {
            continue;
        }
        let (Some(input), Some(output)) = (
            per_million(entry, "input_cost_per_token"),
            per_million(entry, "output_cost_per_token"),
        ) else {
            continue;
        };
        let pattern = normalize_model_name(name);
        if pattern.is_empty() || pattern.len() > 128 || pattern.contains(char::is_whitespace) {
            continue;
        }
        // The first spelling of a model wins, so a dated snapshot never replaces the alias.
        prices.entry(pattern.clone()).or_insert(ModelPrice {
            id: format!("remote:{pattern}"),
            model_pattern: pattern,
            provider_id: None,
            currency: "USD".into(),
            input,
            cache_write: per_million(entry, "cache_creation_input_token_cost"),
            cache_read: per_million(entry, "cache_read_input_token_cost"),
            output,
            source: ModelPriceSource::Remote,
            updated_at: now,
        });
    }
    Ok(prices.into_values().collect())
}

#[cfg(test)]
mod tests {
    use hsin_core::{UsageTokenSummary, best_model_price};

    use super::*;

    #[test]
    fn builtin_table_prices_current_models() {
        let prices = builtin_prices();
        let opus = best_model_price(&prices, None, "claude-opus-5-5[1m]").expect("opus price");
        assert!((opus.input - 4.0).abs() < f64::EPSILON);
        let legacy = best_model_price(&prices, None, "claude-opus-4-1-20250805").expect("legacy");
        assert!((legacy.input - 15.0).abs() < f64::EPSILON);
        let modern = best_model_price(&prices, None, "claude-opus-4-8").expect("modern");
        assert!((modern.input - 5.0).abs() < f64::EPSILON);
        assert!(best_model_price(&prices, None, "gpt-5.4-nano").is_some_and(|p| p.input < 1.0));
        assert!(best_model_price(&prices, None, "unknown-model").is_none());
    }

    #[test]
    fn user_rules_beat_fetched_and_builtin_and_scope_beats_all() {
        let mut prices = builtin_prices();
        let rule = |id: &str, pattern: &str, provider: Option<&str>, source, input| ModelPrice {
            id: id.into(),
            model_pattern: pattern.into(),
            provider_id: provider.map(str::to_owned),
            currency: "CNY".into(),
            input,
            cache_write: None,
            cache_read: None,
            output: 1.0,
            source,
            updated_at: 0,
        };
        prices.push(rule(
            "r",
            "claude-opus-5-5",
            None,
            ModelPriceSource::Remote,
            7.0,
        ));
        prices.push(rule("u", "claude-*", None, ModelPriceSource::User, 8.0));
        prices.push(rule(
            "s",
            "claude-opus-5-5",
            Some("p1"),
            ModelPriceSource::Builtin,
            9.0,
        ));
        let pick = |provider| best_model_price(&prices, provider, "claude-opus-5-5").unwrap();
        assert_eq!(pick(None).id, "u");
        assert_eq!(pick(Some("p1")).id, "s");
        assert_eq!(pick(Some("p2")).id, "u");
    }

    #[test]
    fn cost_falls_back_to_input_price_for_cache_tokens() {
        let price = ModelPrice {
            id: "x".into(),
            model_pattern: "x".into(),
            provider_id: None,
            currency: "USD".into(),
            input: 2.0,
            cache_write: None,
            cache_read: Some(0.5),
            output: 10.0,
            source: ModelPriceSource::User,
            updated_at: 0,
        };
        let tokens = UsageTokenSummary {
            input_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            output_tokens: 100_000,
            reasoning_output_tokens: 50_000,
            request_count: 3,
        };
        assert!((price.cost(&tokens) - (2.0 + 2.0 + 1.0 + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn remote_table_keeps_first_party_chat_models_only() {
        let body = br#"{
            "sample_spec": {"mode": "chat"},
            "gpt-5.5": {"litellm_provider": "openai", "mode": "chat", "input_cost_per_token": 5e-6, "output_cost_per_token": 3e-5, "cache_read_input_token_cost": 5e-7},
            "deepseek/deepseek-v4-pro": {"litellm_provider": "deepseek", "mode": "chat", "input_cost_per_token": 1.32e-6, "output_cost_per_token": 3.96e-6},
            "openrouter/gpt-5.5": {"litellm_provider": "openrouter", "mode": "chat", "input_cost_per_token": 9e-6, "output_cost_per_token": 9e-5},
            "text-embedding-4": {"litellm_provider": "openai", "mode": "embedding", "input_cost_per_token": 1e-7, "output_cost_per_token": 0},
            "claude-broken": {"litellm_provider": "anthropic", "mode": "chat", "input_cost_per_token": "free"}
        }"#;
        let prices = parse_remote(body, 7).unwrap();
        let patterns = prices
            .iter()
            .map(|price| price.model_pattern.as_str())
            .collect::<Vec<_>>();
        assert_eq!(patterns, ["deepseek-v4-pro", "gpt-5.5"]);
        let gpt = &prices[1];
        assert!((gpt.input - 5.0).abs() < 1e-9);
        assert!((gpt.cache_read.unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(gpt.source, ModelPriceSource::Remote);
    }

    #[test]
    fn oversized_or_malformed_tables_are_rejected() {
        assert!(parse_remote(&vec![b' '; MAX_REMOTE_BYTES + 1], 0).is_err());
        assert!(parse_remote(b"[]", 0).is_err());
        assert!(parse_remote(b"{", 0).is_err());
    }
}
