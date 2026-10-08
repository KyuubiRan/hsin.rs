//! Account identities and encrypted OAuth envelopes are committed together.
use super::{
    Database, EncryptedProtectedValue, encode_image_models, encode_model_mapping,
    encode_network_proxy, map_constraint, unix_time, upsert_protected_value,
};
use crate::error::{DaemonError, Result};
use hsin_core::{ClientKind, OfficialAccountSummary, Provider};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct OfficialAccountRecord {
    pub provider_id: String,
    pub account_id: String,
    pub organization_id: String,
    pub credential_revision: u64,
}

pub fn credential_key(provider_id: &str) -> String {
    format!("official_account:{provider_id}")
}

pub(super) fn removed_account_key(
    client: ClientKind,
    account_id: &str,
    organization_id: &str,
) -> String {
    let identity =
        serde_json::to_vec(&(client, account_id, organization_id)).expect("identity serialization");
    format!("official_removed:{}", hex::encode(Sha256::digest(identity)))
}

impl Database {
    pub fn official_account_was_removed(
        &self,
        client: ClientKind,
        account_id: &str,
        organization_id: &str,
    ) -> Result<bool> {
        Ok(self
            .setting(&removed_account_key(client, account_id, organization_id))?
            .is_some())
    }
}

impl Database {
    pub fn official_account(&self, provider_id: &str) -> Result<Option<OfficialAccountRecord>> {
        self.connection.lock().query_row(
            "SELECT provider_id,account_id,organization_id,credential_revision FROM official_accounts WHERE provider_id=?1",
            [provider_id], account_from_row,
        ).optional().map_err(Into::into)
    }

    pub fn find_official_account(
        &self,
        client: ClientKind,
        account_id: &str,
        organization_id: &str,
    ) -> Result<Option<OfficialAccountRecord>> {
        self.connection.lock().query_row(
            "SELECT provider_id,account_id,organization_id,credential_revision FROM official_accounts WHERE client=?1 AND account_id=?2 AND organization_id=?3",
            params![client.as_str(),account_id,organization_id], account_from_row,
        ).optional().map_err(Into::into)
    }

    pub fn save_official_account(
        &self,
        provider: &Provider,
        account: &OfficialAccountRecord,
        summary: &OfficialAccountSummary,
        encrypted: &EncryptedProtectedValue,
    ) -> Result<()> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_time()?;
        if encrypted.key != credential_key(&provider.id)
            || account.provider_id != provider.id
            || !provider.official
            || !summary.saved
        {
            return Err(DaemonError::Invalid(
                "invalid official account envelope".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO providers(id,client,name,description,base_url,auth_scheme,model,revision,official,claude_model_mapping,codex_config_name,scope,codex_image_enabled,codex_image_models,codex_image_preferred_model,network_proxy,codex_tuning,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?18) ON CONFLICT(id) DO UPDATE SET updated_at=excluded.updated_at",
            params![provider.id,provider.client.as_str(),provider.name,provider.description,provider.base_url,provider.auth_scheme.to_string(),provider.model,provider.revision,provider.official,encode_model_mapping(provider)?,provider.codex_config_name,provider.scope.to_string(),provider.codex_image.enabled,encode_image_models(provider)?,provider.codex_image.preferred_model,encode_network_proxy(provider)?,serde_json::to_string(&provider.codex_tuning)?,now],
        ).map_err(map_constraint)?;
        let updated = transaction.execute(
            "INSERT INTO official_accounts(provider_id,client,account_id,organization_id,summary,credential_revision,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(provider_id) DO UPDATE SET summary=excluded.summary,credential_revision=excluded.credential_revision,updated_at=excluded.updated_at WHERE official_accounts.account_id=excluded.account_id AND official_accounts.organization_id=excluded.organization_id AND official_accounts.credential_revision < excluded.credential_revision",
            params![account.provider_id,provider.client.as_str(),account.account_id,account.organization_id,serde_json::to_string(summary)?,account.credential_revision,now],
        ).map_err(map_constraint)?;
        // A stale refresh must not overwrite a later explicit login.
        let revision: u64 = transaction.query_row(
            "SELECT credential_revision FROM official_accounts WHERE provider_id=?1",
            [&provider.id],
            |row| row.get(0),
        )?;
        if updated != 1 || revision != account.credential_revision {
            return Err(DaemonError::Conflict("official credentials changed".into()));
        }
        upsert_protected_value(&transaction, encrypted, now)?;
        transaction.execute(
            "DELETE FROM settings WHERE key=?1",
            [removed_account_key(
                provider.client,
                &account.account_id,
                &account.organization_id,
            )],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OfficialAccountRecord> {
    Ok(OfficialAccountRecord {
        provider_id: row.get(0)?,
        account_id: row.get(1)?,
        organization_id: row.get(2)?,
        credential_revision: row.get(3)?,
    })
}
