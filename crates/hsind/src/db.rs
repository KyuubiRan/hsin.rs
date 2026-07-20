use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use rusqlite::{Connection, DatabaseName, OptionalExtension, TransactionBehavior, params};

use crate::{
    error::{DaemonError, Result},
    model::{AuthScheme, ClientKind, ClientState, ConnectionMode, Provider, ProviderInput},
};

const SCHEMA_VERSION: i64 = 4;

const PROVIDER_COLUMNS: &str = "p.id,p.client,p.name,p.description,p.base_url,p.auth_scheme,p.model,p.revision,p.official,EXISTS(SELECT 1 FROM provider_secrets configured WHERE configured.provider_id=p.id)";

pub struct Database {
    connection: Mutex<Connection>,
}

pub type KeyRecord = (u32, Vec<u8>, Vec<u8>);
pub type PendingOperation = (String, String, ClientKind, Option<String>, String);

#[derive(Debug, Clone)]
pub struct EncryptedSecret {
    pub provider_id: String,
    pub key_version: u32,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct EncryptedProtectedValue {
    pub key: String,
    pub key_version: u32,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl Database {
    pub fn open(path: &Path, backup_dir: &Path) -> Result<Self> {
        if path.exists() {
            let version = database_version(path)?;
            if version > SCHEMA_VERSION {
                return Err(DaemonError::UnsupportedDatabaseVersion(version));
            }
            backup_before_migration(path, backup_dir)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
        )?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn list_providers(&self, client: Option<ClientKind>) -> Result<Vec<Provider>> {
        let connection = self.connection.lock();
        let mut output = Vec::new();
        if let Some(client) = client {
            let sql = format!(
                "SELECT {PROVIDER_COLUMNS} FROM providers p WHERE p.client=?1 ORDER BY p.official DESC, lower(p.name)"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map([client.to_string()], provider_from_row)?;
            for row in rows {
                output.push(row?);
            }
        } else {
            let sql = format!(
                "SELECT {PROVIDER_COLUMNS} FROM providers p ORDER BY p.client, p.official DESC, lower(p.name)"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map([], provider_from_row)?;
            for row in rows {
                output.push(row?);
            }
        }
        Ok(output)
    }

    pub fn get_provider(&self, id: &str) -> Result<Provider> {
        let sql = format!("SELECT {PROVIDER_COLUMNS} FROM providers p WHERE p.id=?1");
        self.connection
            .lock()
            .query_row(&sql, [id], provider_from_row)
            .optional()?
            .ok_or_else(|| DaemonError::NotFound(format!("provider {id}")))
    }

    #[cfg(test)]
    pub fn add_provider(&self, input: &ProviderInput) -> Result<Provider> {
        let provider = Self::new_provider(input)?;
        self.insert_provider(&provider, None)?;
        Ok(provider)
    }

    pub fn new_provider(input: &ProviderInput) -> Result<Provider> {
        input.validate()?;
        Ok(Provider {
            id: uuid::Uuid::new_v4().to_string(),
            client: input.client,
            name: input.normalized_name()?,
            description: input.description.trim().to_owned(),
            base_url: input.base_url.trim().trim_end_matches('/').to_owned(),
            auth_scheme: input.auth_scheme,
            official: false,
            credential_configured: false,
            credential_preview: None,
            model: input.model.as_ref().map(|model| model.trim().to_owned()),
            revision: 1,
        })
    }

    pub fn insert_provider(
        &self,
        provider: &Provider,
        secret: Option<&EncryptedSecret>,
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_time()?;
        transaction.execute(
            "INSERT INTO providers(id,client,name,description,base_url,auth_scheme,model,revision,official,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
            params![provider.id, provider.client.to_string(), provider.name, provider.description, provider.base_url, provider.auth_scheme.to_string(), provider.model, provider.revision, provider.official, now],
        ).map_err(map_constraint)?;
        if let Some(secret) = secret {
            upsert_secret(&transaction, secret, now)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn update_provider(
        &self,
        provider: &Provider,
        expected_revision: u64,
        secret: Option<&EncryptedSecret>,
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_time()?;
        let changed = transaction.execute(
            "UPDATE providers SET name=?1,description=?2,base_url=?3,auth_scheme=?4,model=?5,revision=revision+1,updated_at=?6 WHERE id=?7 AND client=?8 AND revision=?9",
            params![provider.name, provider.description, provider.base_url, provider.auth_scheme.to_string(), provider.model, now, provider.id, provider.client.to_string(), expected_revision],
        ).map_err(map_constraint)?;
        if changed == 0 {
            return Err(DaemonError::Conflict(
                "provider changed or was removed".into(),
            ));
        }
        if let Some(secret) = secret {
            upsert_secret(&transaction, secret, now)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_provider(&self, id: &str) -> Result<()> {
        let connection = self.connection.lock();
        let transaction = connection.unchecked_transaction()?;
        let active: i64 = transaction.query_row(
            "SELECT count(*) FROM client_state WHERE active_provider_id=?1",
            [id],
            |row| row.get(0),
        )?;
        if active > 0 {
            return Err(DaemonError::Conflict(
                "active provider cannot be removed".into(),
            ));
        }
        if transaction.execute("DELETE FROM providers WHERE id=?1", [id])? == 0 {
            return Err(DaemonError::NotFound(format!("provider {id}")));
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn client_state(&self, client: ClientKind) -> Result<ClientState> {
        self.connection
            .lock()
            .query_row(
                "SELECT active_provider_id,mode,config_status FROM client_state WHERE client=?1",
                [client.to_string()],
                |row| {
                    let mode: String = row.get(1)?;
                    let status: String = row.get(2)?;
                    Ok(ClientState {
                        client,
                        active_provider_id: row.get(0)?,
                        mode: ConnectionMode::from_str(&mode).map_err(parse_to_sql_error)?,
                        config_status: parse_config_status(&status).map_err(to_sql_error)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn set_active(
        &self,
        client: ClientKind,
        provider_id: &str,
        config_status: &str,
    ) -> Result<()> {
        let provider = self.get_provider(provider_id)?;
        if provider.client != client {
            return Err(DaemonError::Invalid(
                "provider belongs to a different client".into(),
            ));
        }
        self.connection.lock().execute(
            "UPDATE client_state SET active_provider_id=?1,config_status=?2,updated_at=?3 WHERE client=?4",
            params![provider_id, config_status, unix_time()?, client.to_string()],
        )?;
        Ok(())
    }

    pub fn set_mode(&self, client: ClientKind, mode: ConnectionMode) -> Result<()> {
        self.connection.lock().execute(
            "UPDATE client_state SET mode=?1,updated_at=?2 WHERE client=?3",
            params![mode.to_string(), unix_time()?, client.to_string()],
        )?;
        Ok(())
    }

    pub fn set_config_status(&self, client: ClientKind, status: &str) -> Result<()> {
        self.connection.lock().execute(
            "UPDATE client_state SET config_status=?1,updated_at=?2 WHERE client=?3",
            params![status, unix_time()?, client.to_string()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn put_secret(&self, secret: &EncryptedSecret) -> Result<()> {
        self.connection.lock().execute(
            "INSERT INTO provider_secrets(provider_id,key_version,nonce,ciphertext,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(provider_id) DO UPDATE SET key_version=excluded.key_version,nonce=excluded.nonce,ciphertext=excluded.ciphertext,updated_at=excluded.updated_at",
            params![secret.provider_id, secret.key_version, secret.nonce, secret.ciphertext, unix_time()?],
        )?;
        Ok(())
    }

    pub fn secret(&self, provider_id: &str) -> Result<EncryptedSecret> {
        self.connection.lock().query_row(
            "SELECT provider_id,key_version,nonce,ciphertext FROM provider_secrets WHERE provider_id=?1", [provider_id],
            |row| Ok(EncryptedSecret { provider_id: row.get(0)?, key_version: row.get(1)?, nonce: row.get(2)?, ciphertext: row.get(3)? }),
        ).optional()?.ok_or_else(|| DaemonError::NotFound("provider credential".into()))
    }

    pub fn bound_secret(
        &self,
        client: ClientKind,
        provider_id: &str,
        revision: u64,
    ) -> Result<(Provider, EncryptedSecret)> {
        self.connection
            .lock()
            .query_row(
                "SELECT p.id,p.client,p.name,p.description,p.base_url,p.auth_scheme,p.model,p.revision,p.official,1,s.provider_id,s.key_version,s.nonce,s.ciphertext FROM providers p JOIN provider_secrets s ON s.provider_id=p.id WHERE p.id=?1 AND p.client=?2 AND p.revision=?3",
                params![provider_id, client.to_string(), revision],
                provider_secret_from_row,
            )
            .optional()?
            .ok_or_else(|| DaemonError::Conflict("credential provider binding is stale".into()))
    }

    pub fn active_secret(&self, client: ClientKind) -> Result<(Provider, EncryptedSecret)> {
        self.connection
            .lock()
            .query_row(
                "SELECT p.id,p.client,p.name,p.description,p.base_url,p.auth_scheme,p.model,p.revision,p.official,1,s.provider_id,s.key_version,s.nonce,s.ciphertext FROM client_state c JOIN providers p ON p.id=c.active_provider_id JOIN provider_secrets s ON s.provider_id=p.id WHERE c.client=?1 AND p.client=?1",
                [client.to_string()],
                provider_secret_from_row,
            )
            .optional()?
            .ok_or_else(|| DaemonError::NotFound("active provider credential".into()))
    }

    pub fn all_secrets(&self) -> Result<Vec<EncryptedSecret>> {
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare("SELECT provider_id,key_version,nonce,ciphertext FROM provider_secrets")?;
        let rows = statement.query_map([], |row| {
            Ok(EncryptedSecret {
                provider_id: row.get(0)?,
                key_version: row.get(1)?,
                nonce: row.get(2)?,
                ciphertext: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn protected_value(&self, key: &str) -> Result<Option<EncryptedProtectedValue>> {
        self.connection
            .lock()
            .query_row(
                "SELECT key,key_version,nonce,ciphertext FROM protected_values WHERE key=?1",
                [key],
                |row| {
                    Ok(EncryptedProtectedValue {
                        key: row.get(0)?,
                        key_version: row.get(1)?,
                        nonce: row.get(2)?,
                        ciphertext: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn put_protected_value(&self, value: &EncryptedProtectedValue) -> Result<()> {
        self.connection.lock().execute(
            "INSERT INTO protected_values(key,key_version,nonce,ciphertext,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key) DO UPDATE SET key_version=excluded.key_version,nonce=excluded.nonce,ciphertext=excluded.ciphertext,updated_at=excluded.updated_at",
            params![value.key, value.key_version, value.nonce, value.ciphertext, unix_time()?],
        )?;
        Ok(())
    }

    pub fn delete_protected_value(&self, key: &str) -> Result<()> {
        self.connection
            .lock()
            .execute("DELETE FROM protected_values WHERE key=?1", [key])?;
        Ok(())
    }

    pub fn all_protected_values(&self) -> Result<Vec<EncryptedProtectedValue>> {
        let connection = self.connection.lock();
        let mut statement =
            connection.prepare("SELECT key,key_version,nonce,ciphertext FROM protected_values")?;
        let rows = statement.query_map([], |row| {
            Ok(EncryptedProtectedValue {
                key: row.get(0)?,
                key_version: row.get(1)?,
                nonce: row.get(2)?,
                ciphertext: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn replace_secrets_and_key(
        &self,
        secrets: &[EncryptedSecret],
        protected_values: &[EncryptedProtectedValue],
        old_version: u32,
        version: u32,
        verifier_nonce: &[u8],
        verifier: &[u8],
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for secret in secrets {
            transaction.execute("UPDATE provider_secrets SET key_version=?1,nonce=?2,ciphertext=?3,updated_at=?4 WHERE provider_id=?5", params![secret.key_version,secret.nonce,secret.ciphertext,unix_time()?,secret.provider_id])?;
        }
        for value in protected_values {
            transaction.execute(
                "UPDATE protected_values SET key_version=?1,nonce=?2,ciphertext=?3,updated_at=?4 WHERE key=?5",
                params![value.key_version, value.nonce, value.ciphertext, unix_time()?, value.key],
            )?;
        }
        transaction.execute("INSERT INTO encryption_keys(version,verifier_nonce,verifier,created_at,is_current) VALUES(?1,?2,?3,?4,1)", params![version,verifier_nonce,verifier,unix_time()?])?;
        transaction.execute(
            "UPDATE encryption_keys SET is_current=0 WHERE version<>?1",
            [version],
        )?;
        transaction.execute(
            "INSERT INTO settings(key,value,updated_at) VALUES('key_cleanup_pending',?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![old_version.to_string(), unix_time()?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn current_key_record(&self) -> Result<Option<KeyRecord>> {
        self.connection
            .lock()
            .query_row(
                "SELECT version,verifier_nonce,verifier FROM encryption_keys WHERE is_current=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn initialize_key_record(&self, version: u32, nonce: &[u8], verifier: &[u8]) -> Result<()> {
        self.connection.lock().execute("INSERT OR IGNORE INTO encryption_keys(version,verifier_nonce,verifier,created_at,is_current) VALUES(?1,?2,?3,?4,1)", params![version,nonce,verifier,unix_time()?])?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .lock()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.lock().execute("INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at", params![key,value,unix_time()?])?;
        Ok(())
    }

    pub fn begin_operation(
        &self,
        kind: &str,
        client: ClientKind,
        before_hash: Option<&str>,
        target_json: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.connection.lock().execute("INSERT INTO operations(id,kind,client,state,before_hash,target_json,created_at,updated_at) VALUES(?1,?2,?3,'pending',?4,?5,?6,?6)", params![id,kind,client.to_string(),before_hash,target_json,unix_time()?])?;
        Ok(id)
    }

    pub fn finish_operation(&self, id: &str, state: &str, error: Option<&str>) -> Result<()> {
        self.connection.lock().execute(
            "UPDATE operations SET state=?1,error=?2,updated_at=?3 WHERE id=?4",
            params![state, error, unix_time()?, id],
        )?;
        Ok(())
    }

    pub fn pending_operations(&self) -> Result<Vec<PendingOperation>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare("SELECT id,kind,client,before_hash,target_json FROM operations WHERE state='pending' ORDER BY created_at")?;
        let rows = statement.query_map([], |row| {
            let client: String = row.get(2)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                ClientKind::from_str(&client).map_err(parse_to_sql_error)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn integrity_check(&self) -> Result<String> {
        self.connection
            .lock()
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(Into::into)
    }
}

fn migrate(connection: &Connection) -> Result<()> {
    let mut version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(DaemonError::UnsupportedDatabaseVersion(version));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version == 0 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS providers(id TEXT PRIMARY KEY,client TEXT NOT NULL CHECK(client IN ('codex','claude')),name TEXT NOT NULL,description TEXT NOT NULL DEFAULT '',base_url TEXT NOT NULL,auth_scheme TEXT NOT NULL CHECK(auth_scheme IN ('bearer','x_api_key','oauth')),model TEXT,revision INTEGER NOT NULL,official INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,UNIQUE(client,name));
         CREATE TABLE IF NOT EXISTS provider_secrets(provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,key_version INTEGER NOT NULL,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL,updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS client_state(client TEXT PRIMARY KEY CHECK(client IN ('codex','claude')),active_provider_id TEXT REFERENCES providers(id),mode TEXT NOT NULL CHECK(mode IN ('direct','proxy')),config_status TEXT NOT NULL,updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL,client TEXT NOT NULL,state TEXT NOT NULL,before_hash TEXT,target_json TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS encryption_keys(version INTEGER PRIMARY KEY,verifier_nonce BLOB NOT NULL,verifier BLOB NOT NULL,created_at INTEGER NOT NULL,is_current INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS protected_values(key TEXT PRIMARY KEY,key_version INTEGER NOT NULL,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL,updated_at INTEGER NOT NULL);
         INSERT OR IGNORE INTO client_state(client,mode,config_status,updated_at) VALUES('codex','direct','unmanaged',0),('claude','direct','unmanaged',0);
         INSERT OR IGNORE INTO settings(key,value,updated_at) VALUES('language','system',0),('proxy_port','9999',0),('proxy_enabled','false',0);
         PRAGMA user_version=4;
         COMMIT;"
        )?;
    } else {
        if version == 1 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE providers ADD COLUMN description TEXT NOT NULL DEFAULT '';
                 ALTER TABLE providers ADD COLUMN model TEXT;
                 PRAGMA user_version=2;
                 COMMIT;",
            )?;
            version = 2;
        }
        if version == 2 {
            connection.execute_batch(
                "PRAGMA foreign_keys=OFF;
             BEGIN IMMEDIATE;
             CREATE TABLE providers_v3(id TEXT PRIMARY KEY,client TEXT NOT NULL CHECK(client IN ('codex','claude')),name TEXT NOT NULL,description TEXT NOT NULL DEFAULT '',base_url TEXT NOT NULL,auth_scheme TEXT NOT NULL CHECK(auth_scheme IN ('bearer','x_api_key','oauth')),model TEXT,revision INTEGER NOT NULL,official INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,UNIQUE(client,name));
             INSERT INTO providers_v3(id,client,name,description,base_url,auth_scheme,model,revision,official,created_at,updated_at) SELECT id,client,name,description,base_url,auth_scheme,model,revision,0,created_at,updated_at FROM providers;
             DROP TABLE providers;
             ALTER TABLE providers_v3 RENAME TO providers;
             INSERT OR IGNORE INTO settings(key,value,updated_at) SELECT 'proxy_enabled',CASE WHEN EXISTS(SELECT 1 FROM client_state WHERE mode='proxy') THEN 'true' ELSE 'false' END,0;
             PRAGMA user_version=3;
             COMMIT;
             PRAGMA foreign_keys=ON;",
            )?;
            version = 3;
        }
        if version == 3 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE protected_values(key TEXT PRIMARY KEY,key_version INTEGER NOT NULL,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL,updated_at INTEGER NOT NULL);
                 PRAGMA user_version=4;
                 COMMIT;",
            )?;
        }
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(DaemonError::Database(rusqlite::Error::InvalidQuery));
    }
    Ok(())
}

fn database_version(path: &Path) -> Result<i64> {
    let connection = Connection::open(path)?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn upsert_secret(
    transaction: &rusqlite::Transaction<'_>,
    secret: &EncryptedSecret,
    now: i64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO provider_secrets(provider_id,key_version,nonce,ciphertext,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(provider_id) DO UPDATE SET key_version=excluded.key_version,nonce=excluded.nonce,ciphertext=excluded.ciphertext,updated_at=excluded.updated_at",
        params![secret.provider_id, secret.key_version, secret.nonce, secret.ciphertext, now],
    )?;
    Ok(())
}

fn backup_before_migration(path: &Path, backup_dir: &Path) -> Result<()> {
    fs::create_dir_all(backup_dir)?;
    let source = Connection::open(path)?;
    let version: i64 = source
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let backup = backup_dir.join(format!("hsin-{}.sqlite3", unix_time()?));
    source.backup(DatabaseName::Main, &backup, None)?;
    prune_backups(backup_dir)?;
    Ok(())
}

fn prune_backups(directory: &Path) -> Result<()> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "sqlite3"))
        .collect();
    paths.sort();
    let remove_count = paths.len().saturating_sub(3);
    for path in paths.into_iter().take(remove_count) {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn provider_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    let client: String = row.get(1)?;
    let auth: String = row.get(5)?;
    Ok(Provider {
        id: row.get(0)?,
        client: ClientKind::from_str(&client).map_err(parse_to_sql_error)?,
        name: row.get(2)?,
        description: row.get(3)?,
        base_url: row.get(4)?,
        auth_scheme: AuthScheme::from_str(&auth).map_err(parse_to_sql_error)?,
        model: row.get(6)?,
        revision: row.get(7)?,
        official: row.get(8)?,
        credential_configured: row.get(9)?,
        credential_preview: None,
    })
}

fn provider_secret_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(Provider, EncryptedSecret)> {
    let provider = provider_from_row(row)?;
    let secret = EncryptedSecret {
        provider_id: row.get(10)?,
        key_version: row.get(11)?,
        nonce: row.get(12)?,
        ciphertext: row.get(13)?,
    };
    Ok((provider, secret))
}

fn to_sql_error(error: DaemonError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
fn parse_to_sql_error(error: hsin_core::ParseEnumError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
fn parse_config_status(value: &str) -> Result<crate::model::ConfigStatus> {
    match value {
        "unmanaged" => Ok(crate::model::ConfigStatus::Unmanaged),
        "synchronized" | "managed" => Ok(crate::model::ConfigStatus::Synchronized),
        "drifted" => Ok(crate::model::ConfigStatus::Drifted),
        "conflict" => Ok(crate::model::ConfigStatus::Conflict),
        "unavailable" => Ok(crate::model::ConfigStatus::Unavailable),
        _ => Err(DaemonError::Database(rusqlite::Error::InvalidQuery)),
    }
}
fn map_constraint(error: rusqlite::Error) -> DaemonError {
    if matches!(error, rusqlite::Error::SqliteFailure(ref inner, _) if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
    {
        DaemonError::Conflict("provider name already exists for this client".into())
    } else {
        DaemonError::Database(error)
    }
}
fn unix_time() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DaemonError::Internal(error.to_string()))?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| DaemonError::Internal("system time exceeds SQLite integer range".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_provider_crud() {
        let root = std::env::temp_dir().join(format!("hsind-db-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let db = Database::open(&root.join("db.sqlite"), &root.join("backups")).unwrap();
        assert_eq!(db.integrity_check().unwrap(), "ok");
        assert_eq!(
            db.setting("language").unwrap().as_deref(),
            Some(hsin_core::LANGUAGE_SYSTEM)
        );
        let provider = db
            .add_provider(&ProviderInput {
                client: ClientKind::Codex,
                name: "Test".into(),
                description: "Primary account".into(),
                base_url: "https://example.test/v1".into(),
                auth_scheme: AuthScheme::Bearer,
                model: Some("gpt-test".into()),
            })
            .unwrap();
        assert_eq!(provider.revision, 1);
        let second = Database::new_provider(&ProviderInput {
            client: ClientKind::Claude,
            name: "Atomic".into(),
            description: String::new(),
            base_url: "https://example.test".into(),
            auth_scheme: AuthScheme::XApiKey,
            model: None,
        })
        .unwrap();
        let encrypted = EncryptedSecret {
            provider_id: second.id.clone(),
            key_version: 1,
            nonce: vec![0; 24],
            ciphertext: vec![1, 2, 3],
        };
        db.insert_provider(&second, Some(&encrypted)).unwrap();
        assert_eq!(db.secret(&second.id).unwrap().ciphertext, vec![1, 2, 3]);
        let protected = EncryptedProtectedValue {
            key: "codex_auth_backup_v1".into(),
            key_version: 1,
            nonce: vec![3; 24],
            ciphertext: vec![7, 8, 9],
        };
        db.put_protected_value(&protected).unwrap();
        assert_eq!(
            db.protected_value("codex_auth_backup_v1")
                .unwrap()
                .unwrap()
                .ciphertext,
            vec![7, 8, 9]
        );
        db.delete_protected_value("codex_auth_backup_v1").unwrap();
        assert!(
            db.protected_value("codex_auth_backup_v1")
                .unwrap()
                .is_none()
        );
        let (_, snapshot) = db.bound_secret(ClientKind::Claude, &second.id, 1).unwrap();
        assert_eq!(snapshot.ciphertext, vec![1, 2, 3]);
        let updated = Provider {
            revision: 2,
            base_url: "https://updated.example.test".into(),
            ..second.clone()
        };
        let replacement = EncryptedSecret {
            provider_id: second.id.clone(),
            key_version: 1,
            nonce: vec![2; 24],
            ciphertext: vec![4, 5, 6],
        };
        db.update_provider(&updated, 1, Some(&replacement)).unwrap();
        assert!(db.bound_secret(ClientKind::Claude, &second.id, 1).is_err());
        let (snapshot_provider, snapshot) =
            db.bound_secret(ClientKind::Claude, &second.id, 2).unwrap();
        assert_eq!(snapshot_provider.base_url, updated.base_url);
        assert_eq!(snapshot.ciphertext, vec![4, 5, 6]);
        db.set_active(ClientKind::Codex, &provider.id, "managed")
            .unwrap();
        assert!(db.remove_provider(&provider.id).is_err());
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_schema_is_rejected_without_downgrade() {
        let root = std::env::temp_dir().join(format!("hsind-future-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(matches!(
            Database::open(&path, &root.join("backups")),
            Err(DaemonError::UnsupportedDatabaseVersion(99))
        ));
        assert_eq!(database_version(&path).unwrap(), 99);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_one_database_is_backed_up_and_migrated() {
        let root = std::env::temp_dir().join(format!("hsind-v1-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE providers(id TEXT PRIMARY KEY,client TEXT NOT NULL,name TEXT NOT NULL,base_url TEXT NOT NULL,auth_scheme TEXT NOT NULL,revision INTEGER NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,UNIQUE(client,name));
                 CREATE TABLE provider_secrets(provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,key_version INTEGER NOT NULL,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE client_state(client TEXT PRIMARY KEY,active_provider_id TEXT REFERENCES providers(id),mode TEXT NOT NULL,config_status TEXT NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated_at INTEGER NOT NULL);
                 INSERT INTO providers VALUES('p','codex','Legacy','https://example.test/v1','bearer',1,0,0);
                 INSERT INTO client_state VALUES('codex','p','direct','synchronized',0),('claude',NULL,'direct','unmanaged',0);
                 PRAGMA user_version=1;",
            )
            .unwrap();
        drop(connection);

        let backups = root.join("backups");
        let db = Database::open(&path, &backups).unwrap();
        let provider = db.get_provider("p").unwrap();
        assert_eq!(provider.description, "");
        assert_eq!(provider.model, None);
        assert!(!provider.official);
        assert!(!provider.credential_configured);
        assert_eq!(database_version(&path).unwrap(), 4);
        assert_eq!(fs::read_dir(backups).unwrap().count(), 1);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_migration_preserves_secrets_and_proxy_state() {
        let root = std::env::temp_dir().join(format!("hsind-v2-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE providers(id TEXT PRIMARY KEY,client TEXT NOT NULL,name TEXT NOT NULL,description TEXT NOT NULL DEFAULT '',base_url TEXT NOT NULL,auth_scheme TEXT NOT NULL,model TEXT,revision INTEGER NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,UNIQUE(client,name));
                 CREATE TABLE provider_secrets(provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,key_version INTEGER NOT NULL,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE client_state(client TEXT PRIMARY KEY,active_provider_id TEXT REFERENCES providers(id),mode TEXT NOT NULL,config_status TEXT NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL,client TEXT NOT NULL,state TEXT NOT NULL,before_hash TEXT,target_json TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
                 CREATE TABLE encryption_keys(version INTEGER PRIMARY KEY,verifier_nonce BLOB NOT NULL,verifier BLOB NOT NULL,created_at INTEGER NOT NULL,is_current INTEGER NOT NULL);
                 INSERT INTO providers VALUES('p','codex','Legacy','','https://example.test/v1','bearer',NULL,1,0,0);
                 INSERT INTO provider_secrets VALUES('p',1,X'00',X'01',0);
                 INSERT INTO client_state VALUES('codex','p','proxy','synchronized',0),('claude',NULL,'direct','unmanaged',0);
                 PRAGMA user_version=2;",
            )
            .unwrap();
        drop(connection);

        let db = Database::open(&path, &root.join("backups")).unwrap();
        let provider = db.get_provider("p").unwrap();
        assert!(!provider.official);
        assert!(provider.credential_configured);
        assert_eq!(db.secret("p").unwrap().ciphertext, vec![1]);
        assert_eq!(
            db.setting("proxy_enabled").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(database_version(&path).unwrap(), 4);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_three_migration_adds_protected_values() {
        let root = std::env::temp_dir().join(format!("hsind-v3-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated_at INTEGER NOT NULL);
                 PRAGMA user_version=3;",
            )
            .unwrap();
        drop(connection);

        let db = Database::open(&path, &root.join("backups")).unwrap();
        db.put_protected_value(&EncryptedProtectedValue {
            key: "backup".into(),
            key_version: 1,
            nonce: vec![0; 24],
            ciphertext: vec![1],
        })
        .unwrap();
        assert_eq!(database_version(&path).unwrap(), 4);
        assert!(db.protected_value("backup").unwrap().is_some());
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }
}
