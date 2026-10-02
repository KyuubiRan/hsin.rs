//! Configuration transactions and cooperative transfers between isolated daemons.
use super::{
    App, BTreeMap, CLAUDE_MODEL_ENV_BEFORE_KEY, CODEX_AUTH_BACKUP_KEY, ClientKind, ConfigTarget,
    ConnectionMode, DaemonError, ErrorCode, PathBuf, Result, claude_model_mapping_active, config,
    fs,
};
use crate::ownership::{Guard, ManagedScope, Pending, PendingKind, Record, Target};
use hsin_core::{
    ConfigConflictDetails, ConfigOwnershipStatus, ConfigReleaseParams, ConfigReleaseResult,
    ConfigTakeoverParams, ConfigTakeoverResult, ConfigTakeoverTarget,
};

const AUTH_BACKUP_UNREADABLE: &str =
    "Codex authentication backup could not be decrypted; configuration files were left unchanged";

pub(super) struct ConfigTransaction<'a> {
    app: &'a App,
    directories: Vec<PathBuf>,
}

impl Drop for ConfigTransaction<'_> {
    fn drop(&mut self) {
        let mut guards = self.app.ownership_guards.lock();
        for directory in &self.directories {
            guards.remove(directory);
        }
    }
}

impl App {
    fn ownership_target(&self, client: ClientKind) -> Result<Target> {
        Target::new(client, self.config_path(client)?)
    }

    fn legacy_ownership(&self, target: &Target) -> Result<bool> {
        let residual = config::has_legacy_hsin_configuration(target.client, &target.config_path)?;
        if residual {
            return Ok(true);
        }
        let legacy = (target.client == ClientKind::Codex
            && self.db.protected_value(CODEX_AUTH_BACKUP_KEY)?.is_some())
            || (target.client == ClientKind::Claude
                && self.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?.is_some())
            || self
                .db
                .pending_operations()?
                .iter()
                .any(|(_, kind, client, _, _)| {
                    *client == target.client
                        && matches!(
                            kind.as_str(),
                            "apply_config" | "edit_active_config" | "import_official_auth"
                        )
                });
        Ok(legacy && !config::is_native_recovery_baseline(target.client, &target.config_path)?)
    }

    fn quarantine_native_legacy_state(&self, target: &Target) -> Result<()> {
        if target.read_record()?.is_some()
            || !config::is_native_recovery_baseline(target.client, &target.config_path)?
        {
            return Ok(());
        }
        if target.client == ClientKind::Codex
            && let Some(encrypted) = self.db.protected_value(CODEX_AUTH_BACKUP_KEY)?
        {
            let serialized = zeroize::Zeroizing::new(serde_json::to_vec(&(
                encrypted.key,
                encrypted.key_version,
                encrypted.nonce,
                encrypted.ciphertext,
            ))?);
            let key = format!("quarantined_config_auth:{}", uuid::Uuid::new_v4());
            let envelope = self.crypto.encrypt_protected(&key, &serialized)?;
            self.db
                .quarantine_protected_value(CODEX_AUTH_BACKUP_KEY, &envelope)?;
        }
        if target.client == ClientKind::Claude
            && let Some(snapshot) = self.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?
        {
            self.db.set_setting(
                &format!("quarantined_claude_model_env:{}", uuid::Uuid::new_v4()),
                &snapshot,
            )?;
            self.db.delete_setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?;
        }
        for (id, kind, client, _, json) in self.db.pending_operations()? {
            if client == target.client
                && matches!(
                    kind.as_str(),
                    "apply_config" | "edit_active_config" | "import_official_auth"
                )
                && serde_json::from_str::<ConfigTarget>(&json)
                    .is_ok_and(|operation| operation.ownership_lease.is_none())
            {
                self.db.finish_operation(&id, "quarantined", None)?;
            }
        }
        Ok(())
    }

    fn ownership_problem(
        &self,
        target: &Target,
        record: Option<&Record>,
    ) -> Result<Option<String>> {
        let Some(record) = record else {
            return Ok(self.legacy_ownership(target)?.then(|| "recovery_required: restore native configuration with the previous managing instance or sign in again before claiming ownership".into()));
        };
        if record.pending.is_some() {
            return Ok(Some(
                "recovery_required: a configuration transaction has not completed".into(),
            ));
        }
        let fingerprints =
            config::ownership_fingerprints(target.client, &target.config_path, &record.scope)?;
        Ok((fingerprints != record.fingerprints).then(|| "configuration_changed: managed fields were changed outside the managing instance; automatic restoration is blocked".into()))
    }

    pub(super) fn configuration_ownership(
        &self,
        client: ClientKind,
    ) -> Result<ConfigOwnershipStatus> {
        let target = self.ownership_target(client)?;
        let record = target.read_record()?;
        let reason = self.ownership_problem(&target, record.as_ref())?;
        let mut status = target.status(&self.instance, reason)?;
        if let Some(record) = record
            && record.pending.as_ref().is_some_and(|pending| {
                pending.kind == PendingKind::Takeover
                    && pending
                        .requester
                        .as_ref()
                        .is_some_and(|requester| requester.instance_id == self.instance.instance_id)
            })
        {
            status.takeover_available = true;
            status.takeover_unavailable_reason = None;
        }
        Ok(status)
    }

    pub(super) fn ownership_conflict(&self, clients: &[ClientKind]) -> Result<DaemonError> {
        Ok(DaemonError::Ownership(ConfigConflictDetails {
            targets: clients
                .iter()
                .map(|client| self.configuration_ownership(*client))
                .collect::<Result<_>>()?,
        }))
    }

    /// Preflight every affected directory before changing any persistent state.
    #[allow(clippy::too_many_lines)]
    pub(super) fn begin_config_transaction(
        &self,
        clients: &[ClientKind],
        maintenance: bool,
    ) -> Result<ConfigTransaction<'_>> {
        let mut targets = clients
            .iter()
            .map(|client| self.ownership_target(*client))
            .collect::<Result<Vec<_>>>()?;
        targets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.id.cmp(&right.id))
        });
        targets.dedup_by(|left, right| left.id == right.id);
        let mut existing = self.ownership_guards.lock();
        let mut acquired = BTreeMap::<PathBuf, Guard>::new();
        for target in &targets {
            if !existing.contains_key(&target.path) && !acquired.contains_key(&target.path) {
                acquired.insert(target.path.clone(), target.lock()?);
            }
        }
        let mut conflicts = Vec::new();
        for target in &targets {
            let guard = existing
                .get(&target.path)
                .or_else(|| acquired.get(&target.path))
                .expect("all targets are locked");
            let record = guard.record_for(target);
            let owned = record
                .and_then(|record| record.owner.as_ref())
                .is_some_and(|owner| owner.instance_id == self.instance.instance_id);
            // Reusing a directory lock never authorizes a second, foreign
            // target in that directory. Only preflighted self-owned records
            // can participate in nested writes.
            if existing.contains_key(&target.path) {
                if !owned {
                    conflicts.push(target.client);
                }
                continue;
            }
            let pending_write = owned
                && record
                    .and_then(|record| record.pending.as_ref())
                    .is_some_and(|pending| pending.kind == PendingKind::Write);
            let local_generation = self.db.setting(&format!("config_lease:{}", target.id))?;
            let valid_local = record.is_some_and(|record| {
                local_generation.as_deref() == Some(record.generation.to_string().as_str())
            });
            if (record.is_some() && !owned)
                || (maintenance && (!owned || !valid_local))
                || (!pending_write && self.ownership_problem(target, record)?.is_some())
            {
                conflicts.push(target.client);
            }
            if pending_write
                && !maintenance
                && config::ownership_fingerprints(
                    target.client,
                    &target.config_path,
                    &record.expect("pending record").scope,
                )? != record.expect("pending record").fingerprints
            {
                conflicts.push(target.client);
            }
        }
        if !conflicts.is_empty() {
            drop(existing);
            return Err(self.ownership_conflict(&conflicts)?);
        }
        // Validate every required backup before any target is claimed or any
        // configuration/database state is changed, including Official writes.
        for target in &targets {
            let guard = existing
                .get(&target.path)
                .or_else(|| acquired.get(&target.path))
                .expect("preflight lock");
            if target.client == ClientKind::Codex
                && let Some(record) = guard.record_for(target)
            {
                self.validate_codex_backup_record(target, record)?;
            }
        }
        for target in &targets {
            if existing.contains_key(&target.path) {
                continue;
            }
            let guard = acquired.get_mut(&target.path).expect("new directory lock");
            if guard.record_for(target).is_none() {
                self.quarantine_native_legacy_state(target)?;
                let scope = ManagedScope::default();
                let fingerprints =
                    config::ownership_fingerprints(target.client, &target.config_path, &scope)?;
                let generation = self
                    .db
                    .setting(&format!("config_lease:{}", target.id))?
                    .map(|value| {
                        value.parse::<u64>().map_err(|_| {
                            DaemonError::Config("invalid local configuration generation".into())
                        })
                    })
                    .transpose()?
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| DaemonError::Config("ownership generation exhausted".into()))?;
                let mut record = Record::unclaimed(generation, scope, fingerprints);
                record.owner = Some(self.instance.clone());
                record.endpoint = Some(self.endpoint.read().clone());
                guard.set_record_for(target, record)?;
                self.db.set_setting(
                    &format!("config_lease:{}", target.id),
                    &generation.to_string(),
                )?;
            } else if let Some(mut record) = guard.record_for(target).cloned() {
                // An explicit mutation can finish a claim whose sidecar was
                // durable before the local lease receipt, without stealing it.
                self.db.set_setting(
                    &format!("config_lease:{}", target.id),
                    &record.generation.to_string(),
                )?;
                record.endpoint = Some(self.endpoint.read().clone());
                guard.set_record_for(target, record)?;
            }
        }
        let directories = acquired.keys().cloned().collect();
        existing.extend(acquired);
        Ok(ConfigTransaction {
            app: self,
            directories,
        })
    }

    pub(super) fn ownership_lease(
        &self,
        client: ClientKind,
    ) -> Result<Option<config::AuthBackupLease>> {
        let target = self.ownership_target(client)?;
        let record = self
            .ownership_guards
            .lock()
            .get(&target.path)
            .and_then(|guard| guard.record_for(&target))
            .cloned()
            .or(target.read_record()?);
        Ok(record
            .filter(|record| {
                record
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.instance_id == self.instance.instance_id)
            })
            .map(|record| config::AuthBackupLease {
                instance_id: self.instance.instance_id.clone(),
                target_id: target.id,
                generation: record.generation,
            }))
    }

    fn validate_codex_backup_record(&self, target: &Target, record: &Record) -> Result<()> {
        let required_key = format!(
            "config_auth_backup_required:{}:{}",
            target.id, record.generation
        );
        let Some(encrypted) = self.db.protected_value(CODEX_AUTH_BACKUP_KEY)? else {
            if self.db.setting(&required_key)?.is_some() {
                return Err(DaemonError::Conflict("the managed Codex authentication backup is missing; restore native authentication before continuing".into()));
            }
            return Ok(());
        };
        let plaintext = self.crypto.decrypt_protected(&encrypted)?;
        let snapshot: config::CodexAuthSnapshot =
            serde_json::from_slice(&plaintext).map_err(|_| DaemonError::Crypto)?;
        if snapshot.lease.as_ref().is_none_or(|lease| {
            lease.instance_id != self.instance.instance_id
                || lease.target_id != target.id
                || lease.generation != record.generation
        }) {
            return Err(DaemonError::Conflict(
                "Codex authentication backup belongs to a different configuration lease".into(),
            ));
        }
        Ok(())
    }

    fn managed_scope(target: &ConfigTarget) -> ManagedScope {
        let mut scope = ManagedScope {
            codex_auth: Self::manages_codex_auth(target),
            ..ManagedScope::default()
        };
        if target.client == ClientKind::Codex {
            scope.codex_keys = Self::codex_managed_keys(&target.provider);
        } else if claude_model_mapping_active(&target.provider)
            || target.claude_model_env_before.is_some()
        {
            scope.claude_model_keys = hsin_core::CLAUDE_MODEL_ENV_KEYS
                .iter()
                .filter(|key| {
                    target.claude_model_names_enabled
                        || !hsin_core::CLAUDE_MODEL_NAME_ENV_KEYS.contains(key)
                })
                .map(|key| (*key).to_owned())
                .collect();
        }
        scope
    }

    fn codex_managed_keys(provider: &super::Provider) -> Vec<String> {
        if provider.official {
            return Vec::new();
        }
        let mut keys = Vec::new();
        if provider.model.is_some() {
            keys.push("model".into());
        }
        if provider.codex_tuning.context.enabled {
            keys.extend([
                "model_context_window".into(),
                "model_auto_compact_token_limit".into(),
            ]);
        }
        if provider.codex_tuning.reasoning_effort != hsin_core::CodexReasoningEffort::Unchanged {
            keys.push("model_reasoning_effort".into());
        }
        if provider.codex_tuning.plan_mode_reasoning_effort
            != hsin_core::CodexReasoningEffort::Unchanged
        {
            keys.push("plan_mode_reasoning_effort".into());
        }
        keys
    }

    pub(super) fn start_ownership_write(
        &self,
        target: &ConfigTarget,
        operation: &str,
    ) -> Result<()> {
        self.check_target_lease(target)?;
        let path = self.ownership_target(target.client)?;
        let mut guards = self.ownership_guards.lock();
        let guard = guards.get_mut(&path.path).ok_or_else(|| {
            DaemonError::Conflict("configuration transaction is not locked".into())
        })?;
        let mut record = guard
            .record_for(&path)
            .cloned()
            .ok_or_else(|| DaemonError::Conflict("configuration ownership is missing".into()))?;
        let mut scope = Self::managed_scope(target);
        scope
            .codex_keys
            .extend(record.scope.codex_keys.iter().cloned());
        scope.codex_keys.sort();
        scope.codex_keys.dedup();
        scope
            .claude_model_keys
            .extend(record.scope.claude_model_keys.iter().cloned());
        scope.claude_model_keys.sort();
        scope.claude_model_keys.dedup();
        record.scope = scope;
        record.fingerprints =
            config::ownership_fingerprints(path.client, &path.config_path, &record.scope)?;
        record.pending = Some(Pending {
            request_id: operation.to_owned(),
            kind: PendingKind::Write,
            requester: None,
        });
        guard.set_record_for(&path, record)
    }

    pub(super) fn finish_ownership_write(&self, client: ClientKind) -> Result<()> {
        let path = self.ownership_target(client)?;
        let mut guards = self.ownership_guards.lock();
        let guard = guards.get_mut(&path.path).ok_or_else(|| {
            DaemonError::Conflict("configuration transaction is not locked".into())
        })?;
        let mut record = guard
            .record_for(&path)
            .cloned()
            .ok_or_else(|| DaemonError::Conflict("configuration ownership is missing".into()))?;
        if client == ClientKind::Codex && self.db.protected_value(CODEX_AUTH_BACKUP_KEY)?.is_none()
        {
            record.scope.codex_auth = false;
        }
        if client == ClientKind::Claude {
            if self.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?.is_none() {
                record.scope.claude_model_keys.clear();
            } else if !self.claude_model_names_enabled()? {
                record
                    .scope
                    .claude_model_keys
                    .retain(|key| !hsin_core::CLAUDE_MODEL_NAME_ENV_KEYS.contains(&key.as_str()));
            }
        }
        if client == ClientKind::Codex
            && let Some(provider_id) = self.db.client_state(client)?.active_provider_id
        {
            record.scope.codex_keys =
                Self::codex_managed_keys(&self.db.get_provider(&provider_id)?);
        }
        record.fingerprints =
            config::ownership_fingerprints(client, &path.config_path, &record.scope)?;
        record.pending = None;
        guard.set_record_for(&path, record)
    }

    pub(super) fn check_target_lease(&self, target: &ConfigTarget) -> Result<()> {
        let current = self.ownership_lease(target.client)?;
        if current
            .as_ref()
            .zip(target.ownership_lease.as_ref())
            .is_none_or(|(left, right)| {
                left.instance_id != right.instance_id
                    || left.target_id != right.target_id
                    || left.generation != right.generation
            })
        {
            return Err(self.ownership_conflict(&[target.client])?);
        }
        Ok(())
    }

    pub(super) fn check_auth_backup_lease(
        &self,
        snapshot: &config::CodexAuthSnapshot,
    ) -> Result<()> {
        let lease = self.ownership_lease(ClientKind::Codex)?;
        if lease
            .as_ref()
            .zip(snapshot.lease.as_ref())
            .is_none_or(|(left, right)| {
                left.instance_id != right.instance_id
                    || left.target_id != right.target_id
                    || left.generation != right.generation
            })
        {
            return Err(self.ownership_conflict(&[ClientKind::Codex])?);
        }
        Ok(())
    }

    pub(super) fn image_configuration_impact(
        &self,
        possible_change: bool,
    ) -> Result<Vec<ClientKind>> {
        Ok(
            if possible_change
                && self.db.client_state(ClientKind::Codex)?.mode == ConnectionMode::Proxy
            {
                vec![ClientKind::Codex]
            } else {
                vec![]
            },
        )
    }

    fn prepare_takeover_targets(
        &self,
        params: &ConfigTakeoverParams,
        key: &str,
    ) -> Result<Vec<(Target, ConfigTakeoverTarget, Option<Record>)>> {
        let mut prepared = Vec::new();
        for request in &params.targets {
            let target = self.ownership_target(request.client)?;
            if prepared.iter().any(
                |(previous, _, _): &(Target, ConfigTakeoverTarget, Option<Record>)| {
                    previous.client == target.client
                },
            ) {
                return Err(DaemonError::Invalid("duplicate takeover client".into()));
            }
            let record = target.read_record()?;
            let receipt = self.db.setting(&format!("{key}:{}", target.id))?;
            let completed = record.as_ref().is_some_and(|record| {
                record
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.instance_id == self.instance.instance_id)
                    && receipt.as_deref() == Some(record.generation.to_string().as_str())
            });
            let reserved = record.as_ref().is_some_and(|record| {
                record.pending.as_ref().is_some_and(|pending| {
                    pending.kind == PendingKind::Takeover
                        && pending.requester.as_ref().is_some_and(|requester| {
                            requester.instance_id == self.instance.instance_id
                        })
                        && (pending.request_id == params.request_id
                            || (request.expected_generation == record.generation
                                && request.expected_owner_id.as_deref()
                                    == record
                                        .owner
                                        .as_ref()
                                        .map(|owner| owner.instance_id.as_str())))
                })
            });
            if request.target_id != target.id
                || (!reserved
                    && !completed
                    && (record.as_ref().map_or(0, |record| record.generation)
                        != request.expected_generation
                        || record
                            .as_ref()
                            .and_then(|record| record.owner.as_ref())
                            .map(|owner| &owner.instance_id)
                            != request.expected_owner_id.as_ref()))
            {
                return Err(self.ownership_conflict(&[request.client])?);
            }
            let releasing = record
                .as_ref()
                .and_then(|record| record.pending.as_ref())
                .is_some_and(|pending| {
                    pending.kind == PendingKind::Release
                        && pending.request_id == params.request_id
                        && pending.requester.as_ref().is_some_and(|requester| {
                            requester.instance_id == self.instance.instance_id
                        })
                });
            if reserved
                && record.as_ref().is_some_and(|record| {
                    config::ownership_fingerprints(
                        target.client,
                        &target.config_path,
                        &record.scope,
                    )
                    .is_ok_and(|fingerprints| fingerprints != record.fingerprints)
                })
            {
                return Err(self.ownership_conflict(&[request.client])?);
            }
            if !reserved
                && !releasing
                && self.ownership_problem(&target, record.as_ref())?.is_some()
            {
                return Err(self.ownership_conflict(&[request.client])?);
            }
            prepared.push((target, request.clone(), record));
        }
        Ok(prepared)
    }

    #[allow(clippy::too_many_lines)]
    pub async fn takeover_configuration(
        &self,
        params: ConfigTakeoverParams,
    ) -> Result<ConfigTakeoverResult> {
        if params.request_id.is_empty()
            || params.request_id.len() > 128
            || params.targets.is_empty()
            || params.targets.len() > 2
        {
            return Err(DaemonError::Invalid(
                "takeover requires a request ID and one or two distinct clients".into(),
            ));
        }
        let key = format!("config_takeover:{}", params.request_id);
        let serialized = serde_json::to_string(&params)?;
        let request_key = format!("config_takeover_request:{}", params.request_id);
        if self
            .db
            .setting(&request_key)?
            .is_some_and(|previous| previous != serialized)
        {
            return Err(DaemonError::Invalid(
                "takeover request ID was reused for a different request".into(),
            ));
        }
        if let Some(previous) = self.db.setting(&key)? {
            if previous != serialized {
                return Err(DaemonError::Invalid(
                    "takeover request ID was reused for a different request".into(),
                ));
            }
            return Ok(ConfigTakeoverResult {
                request_id: params.request_id,
                clients: self.status()?.clients,
            });
        }
        let prepared = self.prepare_takeover_targets(&params, &key)?;
        let operation = {
            let _mutation = self.mutation.lock().await;
            self.db.set_setting(&request_key, &serialized)?;
            match self
                .db
                .pending_operations()?
                .into_iter()
                .find(|(_, kind, _, _, json)| kind == "takeover_config" && json == &serialized)
            {
                Some((id, _, _, _, _)) => id,
                None => self.db.begin_operation(
                    "takeover_config",
                    params.targets[0].client,
                    None,
                    &serialized,
                )?,
            }
        };
        // No local mutation or directory lock spans this RPC. Peers can release concurrently.
        for (target, request, record) in &prepared {
            if let Some(record) = record
                && let Some(owner) = &record.owner
                && owner.instance_id != self.instance.instance_id
            {
                let endpoint = record.endpoint.clone().ok_or_else(|| {
                    DaemonError::Config(format!(
                        "{}: managing instance has no IPC endpoint; upgrade it before retrying",
                        target.client
                    ))
                })?;
                let mut peer = hsin_ipc::IpcClient::connect(endpoint).await.map_err(|_| {
                    DaemonError::Config(format!(
                        "{}: managing instance is unavailable; start it and retry takeover",
                        target.client
                    ))
                })?;
                let hello = peer
                    .hello(&hsin_ipc::HelloParams::new(
                        "hsind-config-takeover",
                        env!("CARGO_PKG_VERSION"),
                    ))
                    .await
                    .map_err(|_| {
                        DaemonError::Config(
                            format!("{}: managing instance has an incompatible version; upgrade it and retry", target.client),
                        )
                    })?;
                if hello.instance_id.as_deref() != Some(owner.instance_id.as_str())
                    || !hello
                        .capabilities
                        .iter()
                        .any(|capability| capability == hsin_ipc::capability::CONFIG_OWNERSHIP)
                {
                    return Err(DaemonError::Config(
                        "managing instance identity or capability changed; refresh status".into(),
                    ));
                }
                peer.call::<_, ConfigReleaseResult>(
                    hsin_ipc::method::CONFIG_RELEASE,
                    &ConfigReleaseParams { request_id: params.request_id.clone(), target: request.clone(), requester: self.instance.clone() },
                ).await.map_err(|error| match error {
                    hsin_ipc::TransportError::Rpc(rpc) if rpc.data.as_ref().is_some_and(|data| data.code == ErrorCode::KeyStoreLocked) => DaemonError::Locked,
                    hsin_ipc::TransportError::Rpc(rpc) if rpc.data.as_ref().is_some_and(|data| data.config_conflict.is_some()) => DaemonError::Ownership(rpc.data.and_then(|data| data.config_conflict).expect("structured ownership conflict")),
                    hsin_ipc::TransportError::Rpc(rpc) if rpc.data.as_ref().and_then(|data| data.args.get("message")).is_some_and(|message| message.ends_with(AUTH_BACKUP_UNREADABLE)) => DaemonError::Config(format!("{}: {AUTH_BACKUP_UNREADABLE}", target.client)),
                    _ => DaemonError::Config(format!("{}: managing instance could not restore its configuration; inspect its status, unlock it if needed, and retry", target.client)),
                })?;
            }
            // Claim only the reservation made for this instance, never a newer foreign lease.
            let _mutation = self.mutation.lock().await;
            let mut guard = target.lock()?;
            let current = guard.record().cloned();
            if current.is_none() {
                self.quarantine_native_legacy_state(target)?;
            }
            let mut record = current.clone().unwrap_or(Record::unclaimed(
                0,
                ManagedScope::default(),
                config::ownership_fingerprints(
                    target.client,
                    &target.config_path,
                    &ManagedScope::default(),
                )?,
            ));
            let reserved = record.pending.as_ref().is_some_and(|pending| {
                pending.kind == PendingKind::Takeover
                    && pending
                        .requester
                        .as_ref()
                        .is_some_and(|requester| requester.instance_id == self.instance.instance_id)
                    && (pending.request_id == params.request_id
                        || (request.expected_generation == record.generation
                            && request.expected_owner_id.as_deref()
                                == record
                                    .owner
                                    .as_ref()
                                    .map(|owner| owner.instance_id.as_str())))
            });
            let own = record
                .owner
                .as_ref()
                .is_some_and(|owner| owner.instance_id == self.instance.instance_id);
            if !reserved
                && !own
                && (record.owner.is_some() || record.generation != request.expected_generation)
            {
                return Err(self.ownership_conflict(&[target.client])?);
            }
            if reserved
                && config::ownership_fingerprints(
                    target.client,
                    &target.config_path,
                    &record.scope,
                )? != record.fingerprints
            {
                return Err(self.ownership_conflict(&[target.client])?);
            }
            if self.ownership_problem(target, current.as_ref())?.is_some() && !reserved && !own {
                return Err(self.ownership_conflict(&[target.client])?);
            }
            if !own {
                record.generation = record
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| DaemonError::Config("ownership generation exhausted".into()))?;
            }
            record.owner = Some(self.instance.clone());
            record.endpoint = Some(self.endpoint.read().clone());
            record.pending = Some(Pending {
                request_id: params.request_id.clone(),
                kind: PendingKind::Takeover,
                requester: Some(self.instance.clone()),
            });
            record.fingerprints =
                config::ownership_fingerprints(target.client, &target.config_path, &record.scope)?;
            guard.set_record(record.clone())?;
            self.db.set_setting(
                &format!("config_lease:{}", target.id),
                &record.generation.to_string(),
            )?;
            self.db.set_setting(
                &format!("{key}:{}", target.id),
                &record.generation.to_string(),
            )?;
            self.db.set_config_status(target.client, "unmanaged")?;
            record.pending = None;
            guard.set_record(record)?;
        }
        let _mutation = self.mutation.lock().await;
        self.db.set_setting(&key, &serialized)?;
        self.db.finish_operation(&operation, "complete", None)?;
        Ok(ConfigTakeoverResult {
            request_id: params.request_id,
            clients: self.status()?.clients,
        })
    }

    pub async fn release_configuration(
        &self,
        params: ConfigReleaseParams,
    ) -> Result<ConfigReleaseResult> {
        let _mutation = self.mutation.lock().await;
        self.release_configuration_locked(&params)
    }

    // Keep the restoration and durable reservation order visible together.
    #[allow(clippy::too_many_lines)]
    fn release_configuration_locked(
        &self,
        params: &ConfigReleaseParams,
    ) -> Result<ConfigReleaseResult> {
        let target = self.ownership_target(params.target.client)?;
        if params.target.target_id != target.id
            || params.target.expected_owner_id.as_deref()
                != Some(self.instance.instance_id.as_str())
            || params.requester.instance_id == self.instance.instance_id
            || params.request_id.is_empty()
            || params.request_id.len() > 128
        {
            return Err(DaemonError::Invalid(
                "invalid configuration release request".into(),
            ));
        }
        let mut guard = target.lock()?;
        let Some(mut record) = guard.record().cloned() else {
            return Err(self.ownership_conflict(&[target.client])?);
        };
        let completion_key = format!("config_release_done:{}:{}", params.request_id, target.id);
        let request_key = format!("config_release_request:{}:{}", params.request_id, target.id);
        let serialized = serde_json::to_string(params)?;
        if self
            .db
            .setting(&request_key)?
            .is_some_and(|previous| previous != serialized)
        {
            return Err(DaemonError::Invalid(
                "release request ID was reused for a different request".into(),
            ));
        }
        if let Some(generation) = self.db.setting(&completion_key)? {
            self.complete_configuration_release(params)?;
            return Ok(ConfigReleaseResult {
                request_id: params.request_id.clone(),
                target_id: target.id,
                generation: generation
                    .parse()
                    .map_err(|_| DaemonError::Config("invalid release receipt".into()))?,
            });
        }
        let reserved = record.pending.as_ref().is_some_and(|pending| {
            pending.kind == PendingKind::Takeover
                && pending.request_id == params.request_id
                && pending
                    .requester
                    .as_ref()
                    .is_some_and(|requester| requester.instance_id == params.requester.instance_id)
        });
        if reserved {
            self.db.set_setting(&request_key, &serialized)?;
            self.db
                .set_setting(&completion_key, &record.generation.to_string())?;
            self.complete_configuration_release(params)?;
            return Ok(ConfigReleaseResult {
                request_id: params.request_id.clone(),
                target_id: target.id,
                generation: record.generation,
            });
        }
        if record
            .owner
            .as_ref()
            .is_none_or(|owner| owner.instance_id != self.instance.instance_id)
            || record.generation != params.target.expected_generation
        {
            return Err(self.ownership_conflict(&[target.client])?);
        }
        if !self.crypto.is_unlocked() {
            return Err(DaemonError::Locked);
        }
        let retry = record.pending.as_ref().is_some_and(|pending| {
            pending.kind == PendingKind::Release
                && pending.request_id == params.request_id
                && pending
                    .requester
                    .as_ref()
                    .is_some_and(|requester| requester.instance_id == params.requester.instance_id)
        });
        if !retry && self.ownership_problem(&target, Some(&record))?.is_some() {
            return Err(self.ownership_conflict(&[target.client])?);
        }
        if target.client == ClientKind::Codex {
            self.validate_codex_backup_record(&target, &record)
                .map_err(|error| match error {
                    DaemonError::Crypto => DaemonError::Config(AUTH_BACKUP_UNREADABLE.into()),
                    error => error,
                })?;
        }
        let official = self.ensure_official_provider(target.client)?;
        let restore = self.config_target(&official, ConnectionMode::Direct, None)?;
        let current = if target.config_path.exists() {
            fs::read_to_string(&target.config_path)?
        } else {
            String::new()
        };
        let fingerprints =
            config::ownership_fingerprints(target.client, &target.config_path, &record.scope)?;
        if retry
            && fingerprints.config != record.fingerprints.config
            && config::patch_text_with_credential(&current, &restore, None)? != current
        {
            return Err(self.ownership_conflict(&[target.client])?);
        }
        if record.scope.codex_auth && retry && fingerprints.auth != record.fingerprints.auth {
            let snapshot = self
                .codex_auth_backup()?
                .ok_or(DaemonError::CodexOfficialAuthUnavailable)?;
            self.check_auth_backup_lease(&snapshot)?;
            let auth = fs::read_to_string(config::codex_auth_path(&target.config_path)?)?;
            if config::restore_codex_auth_text(&auth, &snapshot)? != auth {
                return Err(self.ownership_conflict(&[target.client])?);
            }
        }
        let journal = self
            .db
            .pending_operations()?
            .into_iter()
            .find(|(_, kind, _, _, json)| {
                kind == "release_config"
                    && serde_json::from_str::<ConfigReleaseParams>(json).is_ok_and(|saved| {
                        saved.request_id == params.request_id && saved.target == params.target
                    })
            })
            .map(|(id, _, _, _, _)| id);
        let operation = match journal {
            Some(id) => id,
            None => self.db.begin_operation(
                "release_config",
                target.client,
                None,
                &serde_json::to_string(params)?,
            )?,
        };
        self.db.set_setting(&request_key, &serialized)?;
        record.pending = Some(Pending {
            request_id: params.request_id.clone(),
            kind: PendingKind::Release,
            requester: Some(params.requester.clone()),
        });
        guard.set_record(record.clone())?;
        self.ownership_guards
            .lock()
            .insert(target.path.clone(), guard);
        let _transaction = ConfigTransaction {
            app: self,
            directories: vec![target.path.clone()],
        };
        config::apply_with_credential(
            &target.config_path,
            config::file_hash(&target.config_path)?.as_deref(),
            &restore,
            None,
        )?;
        self.apply_codex_auth_target(&restore, None)?;
        let scope = ManagedScope {
            codex_auth: record.scope.codex_auth,
            ..ManagedScope::default()
        };
        let mut reserved = Record::unclaimed(
            record
                .generation
                .checked_add(1)
                .ok_or_else(|| DaemonError::Config("ownership generation exhausted".into()))?,
            scope.clone(),
            config::ownership_fingerprints(target.client, &target.config_path, &scope)?,
        );
        reserved.pending = Some(Pending {
            request_id: params.request_id.clone(),
            kind: PendingKind::Takeover,
            requester: Some(params.requester.clone()),
        });
        self.ownership_guards
            .lock()
            .get_mut(&target.path)
            .expect("release lock")
            .set_record(reserved.clone())?;
        self.db
            .set_setting(&completion_key, &reserved.generation.to_string())?;
        self.db.set_config_status(target.client, "unmanaged")?;
        self.db
            .delete_setting(&format!("config_lease:{}", target.id))?;
        self.remove_codex_auth_backup_if_released(target.client, record.generation)?;
        self.release_claude_model_env_snapshot(&restore)?;
        self.db.finish_operation(&operation, "complete", None)?;
        Ok(ConfigReleaseResult {
            request_id: params.request_id.clone(),
            target_id: target.id,
            generation: reserved.generation,
        })
    }

    fn complete_configuration_release(&self, params: &ConfigReleaseParams) -> Result<()> {
        let lease_key = format!("config_lease:{}", params.target.target_id);
        let lease = self
            .db
            .setting(&lease_key)?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| DaemonError::Config("invalid configuration lease receipt".into()))
            })
            .transpose()?;
        // An old release receipt can be replayed after this instance acquires a new lease.
        // Only clean the generation that was released, including a partially finished cleanup.
        if lease.is_none_or(|generation| generation == params.target.expected_generation) {
            self.db
                .set_config_status(params.target.client, "unmanaged")?;
            self.db.delete_setting(&lease_key)?;
            self.remove_codex_auth_backup_if_released(
                params.target.client,
                params.target.expected_generation,
            )?;
            if params.target.client == ClientKind::Claude {
                self.db.delete_setting(CLAUDE_MODEL_ENV_BEFORE_KEY)?;
            }
        }
        for (id, kind, _, _, json) in self.db.pending_operations()? {
            if kind == "release_config"
                && serde_json::from_str::<ConfigReleaseParams>(&json)
                    .is_ok_and(|saved| saved == *params)
            {
                self.db.finish_operation(&id, "complete", None)?;
            }
        }
        Ok(())
    }

    fn remove_codex_auth_backup_if_released(
        &self,
        client: ClientKind,
        generation: u64,
    ) -> Result<()> {
        if client == ClientKind::Codex
            && self
                .codex_auth_backup()?
                .as_ref()
                .and_then(|snapshot| snapshot.lease.as_ref())
                .is_some_and(|lease| {
                    lease.instance_id == self.instance.instance_id && lease.generation == generation
                })
        {
            self.remove_codex_auth_backup()?;
            let target = self.ownership_target(client)?;
            self.db.delete_setting(&format!(
                "config_auth_backup_required:{}:{}",
                target.id, generation
            ))?;
        }
        Ok(())
    }

    pub(super) fn recover_release_configuration(&self, params: &ConfigReleaseParams) -> Result<()> {
        self.release_configuration_locked(params).map(|_| ())
    }
}
