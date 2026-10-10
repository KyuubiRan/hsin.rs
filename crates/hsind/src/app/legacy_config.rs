//! Adopt a proven local pre-ownership client write without touching client files.
use super::{
    App, CLAUDE_MODEL_ENV_BEFORE_KEY, CODEX_AUTH_BACKUP_KEY, ClientKind, ConfigTarget,
    ConnectionMode, DaemonError, ExposeSecret, Path, PathBuf, Provider, Result, Zeroizing,
    claude_model_mapping_active, config, fs,
};
use crate::{
    db::EncryptedProtectedValue,
    ownership::{Guard, Record, Target},
};

/// The generation of the first record published for a legacy configuration.
const LEGACY_GENERATION: u64 = 1;

/// Why a legacy configuration could not be adopted automatically. The code is
/// persisted per target so ownership status can explain it.
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
    /// The saved Claude model-env values no longer match that write.
    ModelSnapshotMismatch,
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
            Self::ModelSnapshotMismatch => "model_snapshot_mismatch",
            Self::Failed => "migration_failed",
        }
    }
}

pub(super) fn legacy_claim_block_key(target_id: &str) -> String {
    format!("config_legacy_claim_block:{target_id}")
}

/// A legacy write this instance has proven to be its own.
struct LegacyProof {
    configured: ConfigTarget,
    /// Client files that must be unchanged when the claim is staged.
    hashes: Vec<(PathBuf, Option<String>)>,
    /// The Codex authentication backup, rebound to the new lease.
    backup: Option<EncryptedProtectedValue>,
}

enum LegacyEvidence {
    Proven(Box<LegacyProof>),
    Blocked(LegacyClaimBlock),
}

impl App {
    /// Runs before startup reconciliation. A failed proof leaves the existing
    /// ownership recovery restrictions intact and the daemon available over IPC,
    /// and records why so ownership status can explain the recovery.
    pub(crate) fn migrate_legacy_configurations(&self) -> Result<()> {
        // A locked key store only stops the client whose proof needs a secret.
        let codex = self.migrate_legacy_configuration(ClientKind::Codex);
        let claude = self.migrate_legacy_configuration(ClientKind::Claude);
        codex.and(claude)
    }

    fn migrate_legacy_configuration(&self, client: ClientKind) -> Result<()> {
        let mut target = None;
        let block = match self
            .config_path(client)
            .and_then(|path| Target::new(client, path))
            .and_then(|resolved| self.claim_legacy_configuration(target.insert(resolved)))
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
                    client,
                    if matches!(error, DaemonError::Conflict(_)) {
                        "conflict"
                    } else {
                        "unavailable"
                    },
                )?;
                tracing::warn!(
                    %client,
                    code = error.code(),
                    "legacy configuration migration needs recovery"
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
                    %client,
                    reason = block.code(),
                    "legacy configuration could not be adopted automatically"
                );
                self.db.set_setting(&key, block.code())
            }
            None => self.db.delete_setting(&key),
        }
    }

    /// Returns why legacy state still blocks ownership, or `None` once it is
    /// adopted or when there is no legacy hsin configuration to adopt.
    fn claim_legacy_configuration(&self, target: &Target) -> Result<Option<LegacyClaimBlock>> {
        let client = target.client;
        let receipt_key = format!("config_legacy_claim:{}", target.id);
        let receipt = self.db.setting(&receipt_key)?;
        if receipt.is_none()
            && (target.read_record()?.is_some() || !self.legacy_ownership(target)?)
        {
            return Ok(None);
        }
        if receipt.is_none() && self.db.latest_completed_configuration(client)?.is_none() {
            return Ok(Some(LegacyClaimBlock::NoJournal));
        }
        let mut guard = target.lock()?;
        if let Some(receipt) = receipt {
            let record: Record = serde_json::from_str(&receipt)?;
            self.finish_legacy_claim(target, &mut guard, &record)?;
            return Ok(None);
        }
        if guard.record().is_some() {
            return Ok(None);
        }
        if self.legacy_claim_in_flight(target)? {
            return Ok(Some(LegacyClaimBlock::InFlight));
        }
        let Some(operation) = self.db.latest_completed_configuration(client)? else {
            return Ok(Some(LegacyClaimBlock::NoJournal));
        };
        let configured: ConfigTarget = serde_json::from_str(&operation)?;
        let evidence = match client {
            ClientKind::Codex => self.prove_legacy_codex(target, configured)?,
            ClientKind::Claude => self.prove_legacy_claude(target, configured)?,
        };
        let proof = match evidence {
            LegacyEvidence::Proven(proof) => *proof,
            LegacyEvidence::Blocked(block) => return Ok(Some(block)),
        };
        let scope = self.legacy_claim_scope(&proof.configured)?;
        let fingerprints = config::ownership_fingerprints(client, &target.config_path, &scope)?;
        let mut record = Record::unclaimed(LEGACY_GENERATION, scope, fingerprints);
        record.owner = Some(self.instance.clone());
        record.endpoint = Some(self.endpoint.read().clone());
        for (path, hash) in &proof.hashes {
            if config::file_hash(path)? != *hash {
                return Err(DaemonError::Conflict(format!(
                    "{client} configuration changed during legacy migration"
                )));
            }
        }
        self.db.stage_legacy_config_claim(
            &target.id,
            record.generation,
            &serde_json::to_string(&record)?,
            proof.backup.as_ref(),
        )?;
        self.finish_legacy_claim(target, &mut guard, &record)?;
        Ok(None)
    }

    fn prove_legacy_codex(
        &self,
        target: &Target,
        configured: ConfigTarget,
    ) -> Result<LegacyEvidence> {
        let path = &target.config_path;
        let auth_path = config::codex_auth_path(path)?;
        let hashes = vec![
            (path.clone(), config::file_hash(path)?),
            (auth_path.clone(), config::file_hash(&auth_path)?),
        ];
        let current = Zeroizing::new(fs::read_to_string(path)?);
        if !self.legacy_target_matches(ClientKind::Codex, &configured, &current)? {
            return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::StateChanged));
        }
        let credential = self.config_credential(&configured)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        if config::patch_text_with_credential(&current, &configured, credential)? != *current
            || (Self::manages_codex_auth(&configured)
                && !self.codex_auth_target_is_applied(&configured, credential)?)
        {
            return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::FilesChanged));
        }
        let mut backup = self.codex_auth_backup()?;
        if Self::manages_codex_auth(&configured) != backup.is_some() {
            return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::BackupMismatch));
        }
        let backup = match &mut backup {
            Some(backup) => {
                if backup.lease.is_some()
                    || !backup.belongs_to(&auth_path)?
                    || backup.openai_api_key.as_deref() == Some(config::HSIN_MANAGED_KEY)
                {
                    return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::BackupMismatch));
                }
                // Pre-ownership releases recorded the path as the environment
                // spelled it; the rebound backup carries the normalized form.
                backup.auth_path = auth_path.to_string_lossy().into_owned();
                backup.lease = Some(config::AuthBackupLease {
                    instance_id: self.instance.instance_id.clone(),
                    target_id: target.id.clone(),
                    generation: LEGACY_GENERATION,
                });
                Some(self.crypto.encrypt_protected(
                    CODEX_AUTH_BACKUP_KEY,
                    &Zeroizing::new(serde_json::to_vec(backup)?),
                )?)
            }
            None => None,
        };
        Ok(LegacyEvidence::Proven(Box::new(LegacyProof {
            configured,
            hashes,
            backup,
        })))
    }

    fn prove_legacy_claude(
        &self,
        target: &Target,
        configured: ConfigTarget,
    ) -> Result<LegacyEvidence> {
        let path = &target.config_path;
        let hashes = vec![(path.clone(), config::file_hash(path)?)];
        let current = Zeroizing::new(fs::read_to_string(path)?);
        if !self.legacy_target_matches(ClientKind::Claude, &configured, &current)? {
            return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::StateChanged));
        }
        if !self.legacy_claude_snapshot_matches(&configured)? {
            return Ok(LegacyEvidence::Blocked(
                LegacyClaimBlock::ModelSnapshotMismatch,
            ));
        }
        let credential = self.config_credential(&configured)?;
        let credential = credential.as_ref().map(ExposeSecret::expose_secret);
        if config::patch_text_with_credential(&current, &configured, credential)? != *current {
            return Ok(LegacyEvidence::Blocked(LegacyClaimBlock::FilesChanged));
        }
        Ok(LegacyEvidence::Proven(Box::new(LegacyProof {
            configured,
            hashes,
            backup: None,
        })))
    }

    /// The saved model-env values are the only record of what the user had in
    /// the keys hsin owns, so they must still be the ones the legacy write used.
    /// A write that left the mapping restored and released them.
    fn legacy_claude_snapshot_matches(&self, configured: &ConfigTarget) -> Result<bool> {
        let stored = self
            .db
            .setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?
            .map(|stored| serde_json::from_str::<config::ClaudeModelEnvSnapshot>(&stored))
            .transpose()?;
        let expected = configured
            .claude_model_env_before
            .as_ref()
            .filter(|_| claude_model_mapping_active(&configured.provider));
        Ok(match (stored, expected) {
            (None, None) => true,
            (Some(stored), Some(expected)) => {
                serde_json::to_value(&stored)? == serde_json::to_value(expected)?
            }
            _ => false,
        })
    }

    /// Mirrors the scope a completed ownership write would record.
    fn legacy_claim_scope(
        &self,
        configured: &ConfigTarget,
    ) -> Result<crate::ownership::ManagedScope> {
        let mut scope = Self::managed_scope(configured);
        if configured.client == ClientKind::Claude {
            if self.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?.is_none() {
                scope.claude_model_keys.clear();
            } else if !self.claude_model_names_enabled()? {
                scope
                    .claude_model_keys
                    .retain(|key| !hsin_core::CLAUDE_MODEL_NAME_ENV_KEYS.contains(&key.as_str()));
            }
        }
        Ok(scope)
    }

    fn legacy_claim_in_flight(&self, target: &Target) -> Result<bool> {
        Ok(self
            .db
            .setting(&format!("config_lease:{}", target.id))?
            .is_some()
            || self
                .db
                .pending_operations()?
                .iter()
                .any(|(_, _, client, _, _)| *client == target.client))
    }

    fn legacy_target_matches(
        &self,
        client: ClientKind,
        configured: &ConfigTarget,
        current: &str,
    ) -> Result<bool> {
        if configured.client != client
            || configured.provider.client != client
            || configured.provider.official
            || configured.ownership_lease.is_some()
            || Path::new(&configured.credential_command) != self.credential_command
        {
            return Ok(false);
        }
        let state = self.db.client_state(client)?;
        let Some(active_id) = state.active_provider_id else {
            return Ok(false);
        };
        let active = self.db.get_provider(&active_id)?;
        let auth = self.client_auth_settings()?;
        // Legacy proxy switches could change the active route without rewriting
        // the configuration. Its last completed local write still proves which
        // configuration and authentication this instance preserved.
        if active.official
            || state.mode != configured.mode
            || (state.mode == ConnectionMode::Direct && active_id != configured.provider.id)
            || auth.disable_custom_auth(client) != configured.disable_custom_auth
            || (client == ClientKind::Codex
                && auth.codex_preserve_official_auth != configured.codex_preserve_official_auth)
            || (client == ClientKind::Claude
                && !Self::managed_scope(configured).claude_model_keys.is_empty()
                && self.claude_model_names_enabled()? != configured.claude_model_names_enabled)
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
    /// proxy configuration; any persisted field that would must still render
    /// exactly what the journaled write did. Whether the file still holds that
    /// write is checked separately.
    fn legacy_proxy_provider_matches(
        &self,
        configured: &ConfigTarget,
        current: &str,
    ) -> Result<bool> {
        let persisted = match self.db.get_provider(&configured.provider.id) {
            Ok(provider) => provider,
            // The configuration names the removed provider at most in the
            // helper; the active route and the next proxy reconciliation
            // replace it.
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
        Ok(
            config::patch_text_with_credential(current, &rendered, None)?
                == config::patch_text_with_credential(current, configured, None)?,
        )
    }

    fn finish_legacy_claim(
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
            return Err(DaemonError::Conflict(format!(
                "legacy {} configuration claim changed during recovery",
                target.client
            )));
        }
        if target.client == ClientKind::Codex {
            self.validate_codex_backup_record(target, record)?;
        }
        guard.set_record(record.clone())?;
        self.db
            .finish_legacy_config_claim(target.client, &target.id)
    }
}
