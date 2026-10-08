//! Adopt a proven local pre-ownership Codex write without touching client files.
use super::{
    App, CODEX_AUTH_BACKUP_KEY, ClientKind, ConfigTarget, ConnectionMode, DaemonError,
    ExposeSecret, Path, Result, Zeroizing, config, fs,
};
use crate::ownership::{Guard, Record, Target};

impl App {
    /// Runs before startup reconciliation. A failed proof leaves the existing
    /// ownership recovery restrictions intact and the daemon available over IPC.
    pub(crate) fn migrate_legacy_codex_configuration(&self) -> Result<()> {
        match self.claim_legacy_codex_configuration() {
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
                Ok(())
            }
            result => result,
        }
    }

    fn claim_legacy_codex_configuration(&self) -> Result<()> {
        let path = self.config_path(ClientKind::Codex)?;
        let target = Target::new(ClientKind::Codex, &path)?;
        let receipt_key = format!("config_legacy_claim:{}", target.id);
        let receipt = self.db.setting(&receipt_key)?;
        if receipt.is_none() && target.read_record()?.is_some() {
            return Ok(());
        }
        if receipt.is_none()
            && (self
                .db
                .latest_completed_configuration(ClientKind::Codex)?
                .is_none()
                || !config::has_legacy_hsin_configuration(ClientKind::Codex, &path)?)
        {
            return Ok(());
        }
        let mut guard = target.lock()?;
        if let Some(receipt) = receipt {
            let record: Record = serde_json::from_str(&receipt)?;
            return self.finish_legacy_codex_claim(&target, &mut guard, &record);
        }
        if guard.record().is_some()
            || self
                .db
                .setting(&format!("config_lease:{}", target.id))?
                .is_some()
            || self
                .db
                .pending_operations()?
                .iter()
                .any(|(_, _, client, _, _)| *client == ClientKind::Codex)
        {
            return Ok(());
        }
        let Some(operation) = self.db.latest_completed_configuration(ClientKind::Codex)? else {
            return Ok(());
        };
        let configured: ConfigTarget = serde_json::from_str(&operation)?;
        if !self.legacy_codex_target_matches(&configured)? {
            return Ok(());
        }
        let config_hash = config::file_hash(&path)?;
        let auth_path = config::codex_auth_path(&path)?;
        let auth_hash = config::file_hash(&auth_path)?;
        let current = Zeroizing::new(fs::read_to_string(&path)?);
        let credential = self.config_credential(&configured)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        if config::patch_text_with_credential(&current, &configured, credential)? != *current
            || (Self::manages_codex_auth(&configured)
                && !self.codex_auth_target_is_applied(&configured, credential)?)
        {
            return Ok(());
        }
        let mut backup = self.codex_auth_backup()?;
        if Self::manages_codex_auth(&configured) != backup.is_some() {
            return Ok(());
        }
        if let Some(backup) = &backup
            && (backup.lease.is_some()
                || Path::new(&backup.auth_path) != auth_path
                || backup.openai_api_key.as_deref() == Some(config::HSIN_MANAGED_KEY))
        {
            return Ok(());
        }
        let scope = Self::managed_scope(&configured);
        let fingerprints = config::ownership_fingerprints(ClientKind::Codex, &path, &scope)?;
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
        if config::file_hash(&path)? != config_hash || config::file_hash(&auth_path)? != auth_hash {
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
        self.finish_legacy_codex_claim(&target, &mut guard, &record)
    }

    fn legacy_codex_target_matches(&self, configured: &ConfigTarget) -> Result<bool> {
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
        let persisted = self.db.get_provider(&configured.provider.id)?;
        let auth = self.client_auth_settings()?;
        // Legacy proxy switches could change the active route without rewriting
        // the provider-bound helper. Its last completed local write still proves
        // which configuration and authentication this instance preserved.
        Ok(!active.official
            && state.mode == configured.mode
            && (state.mode == ConnectionMode::Proxy || active_id == configured.provider.id)
            && persisted == configured.provider
            && auth.codex_disable_custom_auth == configured.disable_custom_auth
            && auth.codex_preserve_official_auth == configured.codex_preserve_official_auth)
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
