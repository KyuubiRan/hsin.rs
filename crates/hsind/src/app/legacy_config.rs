//! Adopt a proven local pre-ownership Codex write without touching client files.
use super::{
    App, CODEX_AUTH_BACKUP_KEY, ClientKind, ConfigTarget, ConnectionMode, DaemonError,
    ExposeSecret, Path, Provider, Result, Zeroizing, config, fs,
};
use crate::ownership::{Guard, Record, Target};

/// Why a legacy Codex configuration could not be adopted automatically. The
/// code is persisted per target so ownership status can explain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegacyClaimBlock {
    /// No completed local write proves what this instance last applied.
    NoJournal,
    /// An unfinished operation or lease must be recovered first.
    InFlight,
    /// The provider, connection mode or authentication settings moved on.
    StateChanged,
    /// The configuration or authentication files differ from that write.
    FilesChanged,
    /// The preserved authentication backup is missing or belongs elsewhere.
    BackupMismatch,
    /// Reading or verifying the legacy state failed.
    Failed,
}

impl LegacyClaimBlock {
    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::NoJournal => "no_journal",
            Self::InFlight => "in_flight",
            Self::StateChanged => "state_changed",
            Self::FilesChanged => "files_changed",
            Self::BackupMismatch => "backup_mismatch",
            Self::Failed => "migration_failed",
        }
    }
}

pub(super) fn legacy_claim_block_key(target_id: &str) -> String {
    format!("config_legacy_claim_block:{target_id}")
}

impl App {
    /// Runs before startup reconciliation. A failed proof leaves the existing
    /// ownership recovery restrictions intact and the daemon available over IPC,
    /// and records why so ownership status can explain the recovery.
    pub(crate) fn migrate_legacy_codex_configuration(&self) -> Result<()> {
        let mut target = None;
        let block = match self
            .config_path(ClientKind::Codex)
            .and_then(|path| Target::new(ClientKind::Codex, path))
            .and_then(|resolved| self.claim_legacy_codex_configuration(target.insert(resolved)))
        {
            Ok(block) => block,
            Err(
                error @ (DaemonError::Conflict(_)
                | DaemonError::Config(_)
                | DaemonError::Io(_)
                | DaemonError::Json(_)
                | DaemonError::NotFound(_)
                | DaemonError::Crypto),
            ) => {
                self.db.set_config_status(
                    ClientKind::Codex,
                    if matches!(error, DaemonError::Conflict(_)) {
                        "conflict"
                    } else {
                        "unavailable"
                    },
                )?;
                tracing::warn!(
                    code = error.code(),
                    "legacy Codex configuration migration needs recovery"
                );
                Some(LegacyClaimBlock::Failed)
            }
            Err(error) => return Err(error),
        };
        let Some(target) = target else {
            return Ok(());
        };
        let key = legacy_claim_block_key(&target.id);
        match block {
            Some(block) => {
                tracing::warn!(
                    reason = block.code(),
                    "legacy Codex configuration could not be adopted automatically"
                );
                self.db.set_setting(&key, block.code())
            }
            None => self.db.delete_setting(&key),
        }
    }

    /// Returns why legacy state still blocks ownership, or `None` once it is
    /// adopted or when there is no legacy hsin configuration to adopt.
    fn claim_legacy_codex_configuration(
        &self,
        target: &Target,
    ) -> Result<Option<LegacyClaimBlock>> {
        let path = &target.config_path;
        let receipt_key = format!("config_legacy_claim:{}", target.id);
        let receipt = self.db.setting(&receipt_key)?;
        if receipt.is_none()
            && (target.read_record()?.is_some()
                || !config::has_legacy_hsin_configuration(ClientKind::Codex, path)?)
        {
            return Ok(None);
        }
        if receipt.is_none()
            && self
                .db
                .latest_completed_configuration(ClientKind::Codex)?
                .is_none()
        {
            return Ok(Some(LegacyClaimBlock::NoJournal));
        }
        let mut guard = target.lock()?;
        if let Some(receipt) = receipt {
            let record: Record = serde_json::from_str(&receipt)?;
            self.finish_legacy_codex_claim(target, &mut guard, &record)?;
            return Ok(None);
        }
        if guard.record().is_some() {
            return Ok(None);
        }
        if self.legacy_codex_in_flight(target)? {
            return Ok(Some(LegacyClaimBlock::InFlight));
        }
        let Some(operation) = self.db.latest_completed_configuration(ClientKind::Codex)? else {
            return Ok(Some(LegacyClaimBlock::NoJournal));
        };
        let configured: ConfigTarget = serde_json::from_str(&operation)?;
        let config_hash = config::file_hash(path)?;
        let auth_path = config::codex_auth_path(path)?;
        let auth_hash = config::file_hash(&auth_path)?;
        let current = Zeroizing::new(fs::read_to_string(path)?);
        if !self.legacy_codex_target_matches(&configured, &current)? {
            return Ok(Some(LegacyClaimBlock::StateChanged));
        }
        let credential = self.config_credential(&configured)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        if config::patch_text_with_credential(&current, &configured, credential)? != *current
            || (Self::manages_codex_auth(&configured)
                && !self.codex_auth_target_is_applied(&configured, credential)?)
        {
            return Ok(Some(LegacyClaimBlock::FilesChanged));
        }
        let mut backup = self.codex_auth_backup()?;
        if Self::manages_codex_auth(&configured) != backup.is_some() {
            return Ok(Some(LegacyClaimBlock::BackupMismatch));
        }
        if let Some(backup) = &mut backup {
            if backup.lease.is_some()
                || !backup.belongs_to(&auth_path)?
                || backup.openai_api_key.as_deref() == Some(config::HSIN_MANAGED_KEY)
            {
                return Ok(Some(LegacyClaimBlock::BackupMismatch));
            }
            // Pre-ownership releases recorded the path as the environment spelled
            // it; the rebound backup carries the normalized form used from now on.
            backup.auth_path = auth_path.to_string_lossy().into_owned();
        }
        let scope = Self::managed_scope(&configured);
        let fingerprints = config::ownership_fingerprints(ClientKind::Codex, path, &scope)?;
        let mut record = Record::unclaimed(1, scope, fingerprints);
        record.owner = Some(self.instance.clone());
        record.endpoint = Some(self.endpoint.read().clone());
        let encrypted = if let Some(backup) = &mut backup {
            backup.lease = Some(config::AuthBackupLease {
                instance_id: self.instance.instance_id.clone(),
                target_id: target.id.clone(),
                generation: record.generation,
            });
            Some(self.crypto.encrypt_protected(
                CODEX_AUTH_BACKUP_KEY,
                &Zeroizing::new(serde_json::to_vec(backup)?),
            )?)
        } else {
            None
        };
        if config::file_hash(path)? != config_hash || config::file_hash(&auth_path)? != auth_hash {
            return Err(DaemonError::Conflict(
                "Codex configuration changed during legacy migration".into(),
            ));
        }
        self.db.stage_legacy_config_claim(
            &target.id,
            record.generation,
            &serde_json::to_string(&record)?,
            encrypted.as_ref(),
        )?;
        self.finish_legacy_codex_claim(target, &mut guard, &record)?;
        Ok(None)
    }

    fn legacy_codex_in_flight(&self, target: &Target) -> Result<bool> {
        Ok(self
            .db
            .setting(&format!("config_lease:{}", target.id))?
            .is_some()
            || self
                .db
                .pending_operations()?
                .iter()
                .any(|(_, _, client, _, _)| *client == ClientKind::Codex))
    }

    fn legacy_codex_target_matches(
        &self,
        configured: &ConfigTarget,
        current: &str,
    ) -> Result<bool> {
        if configured.client != ClientKind::Codex
            || configured.provider.client != ClientKind::Codex
            || configured.provider.official
            || configured.ownership_lease.is_some()
            || Path::new(&configured.credential_command) != self.credential_command
        {
            return Ok(false);
        }
        let state = self.db.client_state(ClientKind::Codex)?;
        let Some(active_id) = state.active_provider_id else {
            return Ok(false);
        };
        let active = self.db.get_provider(&active_id)?;
        let auth = self.client_auth_settings()?;
        // Legacy proxy switches could change the active route without rewriting
        // the provider-bound helper. Its last completed local write still proves
        // which configuration and authentication this instance preserved.
        if active.official
            || state.mode != configured.mode
            || (state.mode == ConnectionMode::Direct && active_id != configured.provider.id)
            || auth.codex_disable_custom_auth != configured.disable_custom_auth
            || auth.codex_preserve_official_auth != configured.codex_preserve_official_auth
        {
            return Ok(false);
        }
        if state.mode == ConnectionMode::Direct {
            return Ok(self.db.get_provider(&configured.provider.id)? == configured.provider);
        }
        self.legacy_proxy_provider_matches(configured, current)
    }

    /// Legacy proxy mode journaled only edits that rewrote the configuration, so
    /// a later rename, key rotation or removal of the journaled provider still
    /// advanced or dropped its persisted revision. Those edits never reach a
    /// proxy configuration; any persisted field that would must still render the
    /// file exactly as it is.
    fn legacy_proxy_provider_matches(
        &self,
        configured: &ConfigTarget,
        current: &str,
    ) -> Result<bool> {
        let persisted = match self.db.get_provider(&configured.provider.id) {
            Ok(provider) => provider,
            // The helper only names the removed provider; the active route and
            // the next proxy reconciliation replace it.
            Err(DaemonError::NotFound(_)) => return Ok(true),
            Err(error) => return Err(error),
        };
        if persisted.client != configured.provider.client
            || persisted.official
            || persisted.scope != configured.provider.scope
        {
            return Ok(false);
        }
        let mut rendered = configured.clone();
        rendered.provider = Provider {
            revision: configured.provider.revision,
            ..persisted
        };
        Ok(config::patch_text_with_credential(current, &rendered, None)? == current)
    }

    fn finish_legacy_codex_claim(
        &self,
        target: &Target,
        guard: &mut Guard,
        record: &Record,
    ) -> Result<()> {
        if record
            .owner
            .as_ref()
            .is_none_or(|owner| owner.instance_id != self.instance.instance_id)
            || record.pending.is_some()
            || guard.record().is_some_and(|current| current != record)
            || self
                .db
                .setting(&format!("config_lease:{}", target.id))?
                .as_deref()
                != Some(record.generation.to_string().as_str())
            || config::ownership_fingerprints(target.client, &target.config_path, &record.scope)?
                != record.fingerprints
        {
            return Err(DaemonError::Conflict(
                "legacy Codex configuration claim changed during recovery".into(),
            ));
        }
        self.validate_codex_backup_record(target, record)?;
        guard.set_record(record.clone())?;
        self.db.finish_legacy_config_claim(&target.id)
    }
}
