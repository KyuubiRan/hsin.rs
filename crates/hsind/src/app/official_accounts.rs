//! Saved official accounts and recoverable native authentication transitions.
use super::{App, ProtectedValueMutation};
use crate::config::{self, ConfigTarget};
use crate::db::OfficialAccountRecord;
use crate::error::{DaemonError, Result};
use crate::native_auth::{NativeAuthSnapshot, NativeAuthStore};
use hsin_core::{ClientKind, OfficialAccountRenameParams, OfficialAccountSummary, Provider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
struct AppliedAccount {
    store_id: String,
    provider_id: String,
    account_id: String,
    organization_id: String,
    credential_revision: u64,
}

fn credential_key(id: &str) -> String {
    format!("official_account:{id}")
}
fn baseline_key(lease: &config::AuthBackupLease) -> String {
    format!("official_baseline:{}:{}", lease.target_id, lease.generation)
}
fn state_key(lease: &config::AuthBackupLease) -> String {
    format!("official_applied:{}:{}", lease.target_id, lease.generation)
}

impl App {
    pub(super) fn native_auth_store(&self, client: ClientKind) -> Result<NativeAuthStore> {
        // Claude's native Keychain namespace hashes the explicit directory
        // expression, so resolving symlinks here would select another login.
        let path = self
            .config_paths
            .read()
            .get(&client)
            .cloned()
            .ok_or_else(|| DaemonError::Config("missing client configuration path".into()))?;
        NativeAuthStore::with_credentials(
            client,
            &path,
            self.claude_explicit_dir,
            self.native_credentials.clone(),
        )
    }

    fn read_official_snapshot(&self, key: &str) -> Result<NativeAuthSnapshot> {
        let encrypted = self.db.protected_value(key)?.ok_or_else(|| {
            DaemonError::Conflict("official authentication snapshot is missing".into())
        })?;
        let plaintext = self.crypto.decrypt_protected(&encrypted)?;
        serde_json::from_slice(&plaintext).map_err(|_| DaemonError::Crypto)
    }

    fn put_official_snapshot(&self, key: &str, snapshot: &NativeAuthSnapshot) -> Result<()> {
        let plaintext = Zeroizing::new(serde_json::to_vec(snapshot)?);
        self.db
            .put_protected_value(&self.crypto.encrypt_protected(key, &plaintext)?)
    }

    pub(super) async fn save_official_account(
        &self,
        snapshot: &NativeAuthSnapshot,
        automatic: bool,
    ) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        self.save_official_account_locked(snapshot, automatic)
    }

    fn save_official_account_locked(
        &self,
        snapshot: &NativeAuthSnapshot,
        automatic: bool,
    ) -> Result<Provider> {
        let identity = snapshot
            .identity
            .as_ref()
            .ok_or_else(|| DaemonError::Invalid("official OAuth identity is unavailable".into()))?;
        if identity.account_id.is_empty() {
            return Err(DaemonError::Invalid(
                "official account identity is empty".into(),
            ));
        }
        let existing = self.db.find_official_account(
            snapshot.client,
            &identity.account_id,
            &identity.organization_id,
        )?;
        // Observing an unmanaged native store must not overwrite a newer explicit login.
        if automatic && let Some(account) = &existing {
            let imported_revision = self
                .db
                .setting(&format!("official_native_revision:{}", account.provider_id))?;
            if imported_revision.as_deref()
                != Some(account.credential_revision.to_string().as_str())
            {
                return self.db.get_provider(&account.provider_id);
            }
            let stored = self.read_official_snapshot(&credential_key(&account.provider_id))?;
            let unchanged = match snapshot.client {
                ClientKind::Codex => stored.auth.get("tokens") == snapshot.auth.get("tokens"),
                ClientKind::Claude => stored.auth == snapshot.auth,
            };
            if unchanged {
                return self.db.get_provider(&account.provider_id);
            }
        }
        let mut provider = if let Some(account) = &existing {
            self.db.get_provider(&account.provider_id)?
        } else {
            let mut provider = self.ensure_official_provider(snapshot.client)?;
            provider.id = uuid::Uuid::new_v4().to_string();
            provider.name = identity
                .native_name
                .clone()
                .or_else(|| identity.email.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "Official Account".into());
            let original = provider.name.clone();
            let providers = self.db.list_providers(Some(snapshot.client))?;
            let mut suffix = 1_u32;
            while providers
                .iter()
                .any(|other| other.name.eq_ignore_ascii_case(&provider.name))
            {
                suffix += 1;
                provider.name = format!("{original} ({suffix})");
            }
            provider
        };
        let summary = OfficialAccountSummary {
            email: identity.email.clone(),
            native_name: identity.native_name.clone(),
            saved: true,
        };
        provider.official_account = Some(summary.clone());
        provider.credential_configured = true;
        let account = OfficialAccountRecord {
            provider_id: provider.id.clone(),
            account_id: identity.account_id.clone(),
            organization_id: identity.organization_id.clone(),
            credential_revision: existing
                .map_or(1, |account| account.credential_revision.saturating_add(1)),
        };
        let mut normalized = snapshot.clone();
        if snapshot.client == ClientKind::Codex {
            let auth = normalized
                .auth
                .as_object_mut()
                .ok_or_else(|| DaemonError::Invalid("invalid official authentication".into()))?;
            auth.insert("auth_mode".into(), serde_json::json!("chatgpt"));
            auth.insert("OPENAI_API_KEY".into(), serde_json::Value::Null);
        }
        let plaintext = Zeroizing::new(serde_json::to_vec(&normalized)?);
        let encrypted = self
            .crypto
            .encrypt_protected(&credential_key(&provider.id), &plaintext)?;
        self.db
            .save_official_account(&provider, &account, &summary, &encrypted)?;
        if automatic {
            self.db.set_setting(
                &format!("official_native_revision:{}", provider.id),
                &account.credential_revision.to_string(),
            )?;
        }
        self.db.get_provider(&provider.id)
    }

    pub async fn capture_existing_official_accounts(&self) -> Result<()> {
        let _guard = self.mutation.lock().await;
        if !self.crypto.is_unlocked() {
            return Ok(());
        }
        for client in ClientKind::ALL {
            let result = (|| -> Result<()> {
                let target = crate::ownership::Target::new(client, self.config_path(client)?)?;
                let target_guard = target.lock()?;
                let record = target_guard.record_for(&target);
                if record.as_ref().is_some_and(|record| {
                    record.pending.is_some()
                        || record
                            .owner
                            .as_ref()
                            .is_some_and(|owner| owner.instance_id != self.instance.instance_id)
                }) {
                    return Ok(());
                }
                let store = self.native_auth_store(client)?;
                let _native_guard = store.lock_for_switch()?;
                let snapshot = store.capture()?;
                if snapshot.identity.is_none() {
                    return Ok(());
                }
                if let Some(lease) = self.ownership_lease(client)? {
                    self.capture_outgoing_account(&store, &snapshot, &lease)?;
                }
                self.capture_native_official_account(&snapshot)?;
                let baseline = self
                    .ownership_lease(client)?
                    .map(|lease| baseline_key(&lease));
                let original = baseline
                    .filter(|key| self.db.protected_value(key).ok().flatten().is_some())
                    .map(|key| self.read_official_snapshot(&key))
                    .transpose()?;
                let displayed = original.as_ref().unwrap_or(&snapshot);
                let Some(identity) = displayed.identity.as_ref() else {
                    return Ok(());
                };
                let summary = OfficialAccountSummary {
                    email: identity.email.clone(),
                    native_name: identity.native_name.clone(),
                    saved: false,
                };
                self.db.set_setting(
                    &format!("official_native_summary:{client}"),
                    &serde_json::to_string(&summary)?,
                )?;
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(client=%client,code=error.code(),"native official account inspection is unavailable");
            }
        }
        Ok(())
    }

    pub async fn rename_official_account(
        &self,
        params: OfficialAccountRenameParams,
    ) -> Result<Provider> {
        let _guard = self.mutation.lock().await;
        let mut provider = self.db.get_provider(&params.provider_id)?;
        if !provider.official || self.db.official_account(&provider.id)?.is_none() {
            return Err(DaemonError::Invalid(
                "provider is not a saved official account".into(),
            ));
        }
        if params.name.trim().is_empty()
            || params.name.chars().count() > 128
            || params.name.chars().any(char::is_control)
        {
            return Err(DaemonError::Invalid(
                "account name must contain 1 to 128 characters without control characters".into(),
            ));
        }
        params.name.trim().clone_into(&mut provider.name);
        self.db.update_provider(
            &provider,
            params.expected_revision,
            None,
            ProtectedValueMutation::Preserve,
        )?;
        self.db.get_provider(&provider.id)
    }

    fn capture_native_official_account(&self, snapshot: &NativeAuthSnapshot) -> Result<()> {
        if let Some(identity) = &snapshot.identity
            && !self.db.official_account_was_removed(
                snapshot.client,
                &identity.account_id,
                &identity.organization_id,
            )?
        {
            self.save_official_account_locked(snapshot, true)?;
        }
        Ok(())
    }

    fn applied_account(&self, lease: &config::AuthBackupLease) -> Result<Option<AppliedAccount>> {
        self.db
            .setting(&state_key(lease))?
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    fn validate_applied_account(
        &self,
        store: &NativeAuthStore,
        snapshot: &NativeAuthSnapshot,
        lease: &config::AuthBackupLease,
    ) -> Result<()> {
        if let Some(applied) = self.applied_account(lease)? {
            let identity = snapshot.identity.as_ref().ok_or_else(|| {
                DaemonError::Conflict("managed official login was removed outside hsin".into())
            })?;
            if applied.store_id != store.store_id()
                || identity.account_id != applied.account_id
                || identity.organization_id != applied.organization_id
            {
                return Err(DaemonError::Conflict(
                    "official account was changed outside hsin".into(),
                ));
            }
        }
        Ok(())
    }

    fn capture_outgoing_account(
        &self,
        store: &NativeAuthStore,
        snapshot: &NativeAuthSnapshot,
        lease: &config::AuthBackupLease,
    ) -> Result<()> {
        self.validate_applied_account(store, snapshot, lease)?;
        let key = baseline_key(lease);
        if self.db.protected_value(&key)?.is_some() {
            let mut baseline = self.read_official_snapshot(&key)?;
            if baseline
                .identity
                .as_ref()
                .zip(snapshot.identity.as_ref())
                .is_some_and(|(first, second)| {
                    first.account_id == second.account_id
                        && first.organization_id == second.organization_id
                })
            {
                if snapshot.client == ClientKind::Codex {
                    for field in ["tokens", "last_refresh"] {
                        if let Some(value) = snapshot.auth.get(field) {
                            baseline.auth[field] = value.clone();
                        }
                    }
                    baseline.identity.clone_from(&snapshot.identity);
                } else {
                    baseline = snapshot.clone();
                }
                self.put_official_snapshot(&key, &baseline)?;
            }
        }
        if let Some(applied) = self.applied_account(lease)?
            && let Some(account) = self.db.official_account(&applied.provider_id)?
            && account.credential_revision == applied.credential_revision
        {
            let stored = self.read_official_snapshot(&credential_key(&applied.provider_id))?;
            // Ignore API-key wrapper changes; only native OAuth refreshes update the vault.
            let changed = match snapshot.client {
                ClientKind::Codex => stored.auth.get("tokens") != snapshot.auth.get("tokens"),
                ClientKind::Claude => stored.auth != snapshot.auth,
            };
            if changed {
                let provider = self.save_official_account_locked(snapshot, false)?;
                let updated = self
                    .db
                    .official_account(&provider.id)?
                    .expect("saved account");
                let next = AppliedAccount {
                    credential_revision: updated.credential_revision,
                    ..applied
                };
                self.db
                    .set_setting(&state_key(lease), &serde_json::to_string(&next)?)?;
            }
        }
        Ok(())
    }

    // Keep the outgoing capture, baseline and immutable journal snapshot in order.
    #[allow(clippy::too_many_lines)]
    pub(super) fn prepare_official_auth_transition(
        &self,
        provider: &Provider,
    ) -> Result<Option<config::OfficialAuthTransition>> {
        let lease = self.ownership_lease(provider.client)?;
        let saved = self.db.official_account(&provider.id)?;
        let Some(lease) = lease else {
            if saved.is_some() {
                return Err(DaemonError::Conflict(
                    "official switch requires a configuration lease".into(),
                ));
            }
            return Ok(None);
        };
        let baseline = baseline_key(&lease);
        let has_baseline = self.db.protected_value(&baseline)?.is_some();
        if saved.is_none() && !has_baseline {
            return Ok(None);
        }
        super::official_login::ensure_native_account_compatibility(provider.client)?;
        let store = self.native_auth_store(provider.client)?;
        let _guard = store.lock_for_switch()?;
        let (before, before_fingerprint, before_source) = store.capture_for_switch()?;
        self.capture_outgoing_account(&store, &before, &lease)?;
        if before.identity.is_some() {
            self.capture_native_official_account(&before)?;
        }
        if !has_baseline {
            let mut original = before.clone();
            if provider.client == ClientKind::Codex
                && let Some(backup) = self.codex_auth_backup()?
            {
                let auth = original.auth.as_object_mut().ok_or(DaemonError::Crypto)?;
                for (key, value) in [
                    ("auth_mode", backup.auth_mode.as_ref()),
                    ("OPENAI_API_KEY", backup.openai_api_key.as_ref()),
                ] {
                    if let Some(value) = value {
                        auth.insert(key.into(), serde_json::json!(value));
                    } else {
                        auth.remove(key);
                    }
                }
            }
            self.put_official_snapshot(&baseline, &original)?;
        }
        if !provider.official {
            return Ok(None);
        }
        let (after, state, restoring_native) = if let Some(account) =
            self.db.official_account(&provider.id)?
        {
            let after = self.read_official_snapshot(&credential_key(&provider.id))?;
            let identity = after.identity.as_ref().ok_or(DaemonError::Crypto)?;
            if after.client != provider.client
                || identity.account_id != account.account_id
                || identity.organization_id != account.organization_id
            {
                return Err(DaemonError::Crypto);
            }
            let state = AppliedAccount {
                store_id: store.store_id(),
                provider_id: provider.id.clone(),
                account_id: account.account_id,
                organization_id: account.organization_id,
                credential_revision: account.credential_revision,
            };
            (after, Some(serde_json::to_string(&state)?), false)
        } else {
            let mut original = self.read_official_snapshot(&baseline)?;
            if let Some(identity) = &original.identity
                && let Some(account) = self.db.find_official_account(
                    provider.client,
                    &identity.account_id,
                    &identity.organization_id,
                )?
            {
                let newest = self.read_official_snapshot(&credential_key(&account.provider_id))?;
                if provider.client == ClientKind::Codex {
                    if let Some(tokens) = newest.auth.get("tokens") {
                        original.auth["tokens"] = tokens.clone();
                    }
                    if let Some(refresh) = newest.auth.get("last_refresh") {
                        original.auth["last_refresh"] = refresh.clone();
                    }
                    original.identity.clone_from(&newest.identity);
                } else {
                    original = newest;
                }
            }
            (original, None, true)
        };
        let id = uuid::Uuid::new_v4();
        let before_key = format!("official_transition:{id}:before");
        let after_key = format!("official_transition:{id}:after");
        self.put_official_snapshot(&before_key, &before)?;
        self.put_official_snapshot(&after_key, &after)?;
        Ok(Some(config::OfficialAuthTransition {
            store_id: store.store_id(),
            before_key,
            after_key,
            before_fingerprint,
            before_source: Some(before_source),
            after_fingerprint: snapshot_fingerprint(&after)?,
            state,
            restoring_native,
        }))
    }

    pub(super) fn apply_official_auth_transition(&self, target: &ConfigTarget) -> Result<()> {
        let Some(transition) = &target.official_auth_transition else {
            return Ok(());
        };
        self.check_target_lease(target)?;
        if let Some(state) = &transition.state {
            let applied: AppliedAccount = serde_json::from_str(state)?;
            let account = self
                .db
                .official_account(&applied.provider_id)?
                .ok_or_else(|| {
                    DaemonError::Conflict(
                        "official account was removed during switch recovery".into(),
                    )
                })?;
            if account.credential_revision != applied.credential_revision
                || account.account_id != applied.account_id
                || account.organization_id != applied.organization_id
            {
                return Err(DaemonError::Conflict(
                    "official credentials were replaced after this switch was prepared".into(),
                ));
            }
        }
        let store = self.native_auth_store(target.client)?;
        if store.store_id() != transition.store_id {
            return Err(DaemonError::Conflict(
                "native authentication target changed".into(),
            ));
        }
        let _guard = store.lock_for_switch()?;
        let before = self.read_official_snapshot(&transition.before_key)?;
        let after = self.read_official_snapshot(&transition.after_key)?;
        store.recover_apply(&before, &after, &transition.before_fingerprint)?;
        if snapshot_fingerprint(&store.capture()?)? != snapshot_fingerprint(&after)? {
            return Err(DaemonError::Conflict(
                "official authentication switch could not be verified".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn rollback_official_auth_transition(&self, target: &ConfigTarget) -> Result<()> {
        let Some(transition) = &target.official_auth_transition else {
            return Ok(());
        };
        let store = self.native_auth_store(target.client)?;
        if store.store_id() != transition.store_id {
            return Err(DaemonError::Conflict(
                "native authentication target changed".into(),
            ));
        }
        let _guard = store.lock_for_switch()?;
        let before = self.read_official_snapshot(&transition.before_key)?;
        let after = self.read_official_snapshot(&transition.after_key)?;
        store.recover_rollback(
            &before,
            &after,
            &transition.before_fingerprint,
            transition.before_source.as_deref().unwrap_or(""),
        )
    }

    pub(super) fn commit_official_auth_transition(&self, target: &ConfigTarget) -> Result<()> {
        let Some(transition) = &target.official_auth_transition else {
            return Ok(());
        };
        let lease = target.ownership_lease.as_ref().ok_or(DaemonError::Crypto)?;
        if let Some(state) = &transition.state {
            self.db.set_setting(&state_key(lease), state)?;
        } else {
            self.db.delete_setting(&state_key(lease))?;
        }
        Ok(())
    }

    pub(super) fn official_auth_transition_is_applied(
        &self,
        target: &ConfigTarget,
    ) -> Result<bool> {
        let Some(transition) = &target.official_auth_transition else {
            return Ok(true);
        };
        let store = self.native_auth_store(target.client)?;
        if store.store_id() != transition.store_id {
            return Ok(false);
        }
        let after = self.read_official_snapshot(&transition.after_key)?;
        Ok(snapshot_fingerprint(&store.capture()?)? == snapshot_fingerprint(&after)?)
    }

    pub(super) fn official_identity_fingerprint(
        &self,
        client: ClientKind,
        store_id: &str,
    ) -> Result<String> {
        let store = self.native_auth_store(client)?;
        if store.store_id() != store_id {
            return Err(DaemonError::Conflict(
                "native credential store changed".into(),
            ));
        }
        store.ownership_fingerprint()
    }

    pub(super) fn official_scope_store(&self, target: &ConfigTarget) -> Result<Option<String>> {
        if let Some(transition) = &target.official_auth_transition {
            return Ok(Some(transition.store_id.clone()));
        }
        if let Some(lease) = &target.ownership_lease
            && self.db.protected_value(&baseline_key(lease))?.is_some()
        {
            return Ok(Some(self.native_auth_store(target.client)?.store_id()));
        }
        Ok(None)
    }

    pub(super) fn clean_official_transition(&self, target: &ConfigTarget) -> Result<()> {
        if let Some(transition) = &target.official_auth_transition {
            self.db.delete_protected_value(&transition.before_key)?;
            self.db.delete_protected_value(&transition.after_key)?;
        }
        Ok(())
    }

    pub(super) fn clear_official_lease(
        &self,
        client: ClientKind,
        target_id: &str,
        generation: u64,
    ) -> Result<()> {
        let lease = config::AuthBackupLease {
            instance_id: self.instance.instance_id.clone(),
            target_id: target_id.to_owned(),
            generation,
        };
        self.db.delete_setting(&state_key(&lease))?;
        self.db.delete_protected_value(&baseline_key(&lease))?;
        self.db
            .delete_setting(&format!("official_native_summary:{client}"))?;
        Ok(())
    }
}

fn snapshot_fingerprint(snapshot: &NativeAuthSnapshot) -> Result<String> {
    Ok(hex::encode(Sha256::digest(Zeroizing::new(
        serde_json::to_vec(&(snapshot.client, &snapshot.auth, &snapshot.metadata))?,
    ))))
}

#[cfg(test)]
mod tests;
