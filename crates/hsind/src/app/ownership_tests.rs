// Boolean authentication assertions keep keys and OAuth tokens out of panic diagnostics.
#![allow(clippy::manual_assert_eq)]

use super::*;

use hsin_core::{
    ClaudeModelMapping, ConfigOwnershipStatus, ConfigReleaseParams, ConfigTakeoverParams,
    ConfigTakeoverTarget, ModelSlot, ProviderDraft, ProviderPatch,
};
use hsin_ipc::{HelloParams, IpcClient, IpcEndpoint, capability};
use parking_lot::Mutex as ParkingMutex;
use serde_json::{Value, json};

use crate::ownership::{ManagedScope, Pending, PendingKind, Record, Target};

#[derive(Default)]
struct MemoryStore(ParkingMutex<HashMap<u32, String>>);

impl KeyStore for MemoryStore {
    fn load(&self, version: u32) -> Result<Option<String>> {
        Ok(self.0.lock().get(&version).cloned())
    }

    fn store(&self, version: u32, value: &str) -> Result<()> {
        self.0.lock().insert(version, value.to_owned());
        Ok(())
    }

    fn delete(&self, version: u32) -> Result<()> {
        self.0.lock().remove(&version);
        Ok(())
    }
}

struct TemporaryRoot(PathBuf);

impl Drop for TemporaryRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Instances {
    first: Arc<App>,
    second: Arc<App>,
    first_store: Arc<MemoryStore>,
    temporary: TemporaryRoot,
}

impl Instances {
    fn new() -> Self {
        // Keep Unix socket paths below the platform's path length limit, including on macOS.
        #[cfg(unix)]
        let parent = PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let parent = std::env::temp_dir();
        let root = parent.join(format!("hsown-{}", uuid::Uuid::new_v4()));
        let codex_home = root.join("codex");
        let claude_home = root.join("claude");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&claude_home).unwrap();
        fs::write(
            codex_home.join("config.toml"),
            "# user configuration\nmodel_provider = \"openai\"\nmodel = \"user-model\"\n",
        )
        .unwrap();
        fs::write(
            codex_home.join("auth.json"),
            "{\n  \"auth_mode\": \"chatgpt\",\n  \"tokens\": {\"access_token\": \"initial-token\"},\n  \"account_id\": \"user-account\"\n}\n",
        )
        .unwrap();
        fs::write(
            claude_home.join("settings.json"),
            "{\n  \"env\": {\n    \"ANTHROPIC_MODEL\": \"user-default\",\n    \"ANTHROPIC_DEFAULT_OPUS_MODEL_NAME\": \"User Opus\"\n  },\n  \"permissions\": {\"allow\": [\"Read\"]}\n}\n",
        )
        .unwrap();
        let open = |name: &str, store: Arc<MemoryStore>| {
            let app = App::open_inner(
                &Paths::for_home(root.join(name)),
                store,
                Some(&codex_home),
                Some(&claude_home),
            )
            .unwrap();
            #[cfg(unix)]
            let endpoint = IpcEndpoint::filesystem(root.join(format!("{name}.sock")));
            #[cfg(not(unix))]
            let endpoint =
                IpcEndpoint::namespaced(format!("hsown-{}-{name}", uuid::Uuid::new_v4()));
            *app.endpoint.write() = endpoint;
            app
        };
        let first_store = Arc::new(MemoryStore::default());
        Self {
            first: open("first", first_store.clone()),
            second: open("second", Arc::new(MemoryStore::default())),
            first_store,
            temporary: TemporaryRoot(root),
        }
    }

    fn reopen_first(&self) -> Arc<App> {
        let app = App::open_inner(
            &Paths::for_home(self.temporary.0.join("first")),
            self.first_store.clone(),
            Some(&self.temporary.0.join("codex")),
            Some(&self.temporary.0.join("claude")),
        )
        .unwrap();
        *app.endpoint.write() = self.first.endpoint.read().clone();
        assert_eq!(app.instance.instance_id, self.first.instance.instance_id);
        app
    }

    fn codex_auth(&self) -> PathBuf {
        self.temporary.0.join("codex/auth.json")
    }

    fn codex_config(&self) -> PathBuf {
        self.temporary.0.join("codex/config.toml")
    }

    fn claude_config(&self) -> PathBuf {
        self.temporary.0.join("claude/settings.json")
    }

    fn owner_record(&self, client: ClientKind) -> Vec<u8> {
        let directory = match client {
            ClientKind::Codex => "codex",
            ClientKind::Claude => "claude",
        };
        fs::read(
            self.temporary
                .0
                .join(directory)
                .join(".hsin-config-owner.json"),
        )
        .unwrap()
    }
}

fn draft(client: ClientKind, name: &str) -> ProviderDraft {
    ProviderDraft {
        client,
        name: name.into(),
        description: String::new(),
        base_url: match client {
            ClientKind::Codex => "https://ownership.example.test/v1",
            ClientKind::Claude => "https://ownership.example.test",
        }
        .into(),
        auth_scheme: match client {
            ClientKind::Codex => AuthScheme::Bearer,
            ClientKind::Claude => AuthScheme::XApiKey,
        },
        model: None,
        codex_config_name: None,
        claude_model_mapping: None,
        scope: ProviderScope::Primary,
        codex_image: hsin_core::CodexImageConfig::default(),
        codex_tuning: hsin_core::CodexTuningSettings::default(),
        network_proxy: ProviderProxyConfig::default(),
    }
}

async fn add(app: &App, provider: ProviderDraft) -> Provider {
    app.add_provider(ProviderAddParams {
        provider,
        secret: SecretInput::Replace("fixture-api-key".into()),
        proxy_password: SecretInput::Preserve,
    })
    .await
    .unwrap()
}

async fn activate(app: &App, provider: &Provider) {
    app.switch_provider(ProviderSwitchParams {
        client: provider.client,
        provider_id: provider.id.clone(),
    })
    .await
    .unwrap();
}

fn ownership(app: &App, client: ClientKind) -> ConfigOwnershipStatus {
    app.status()
        .unwrap()
        .clients
        .into_iter()
        .find(|state| state.client == client)
        .unwrap()
        .config_ownership
        .unwrap()
}

fn takeover(app: &App, clients: &[ClientKind]) -> ConfigTakeoverParams {
    ConfigTakeoverParams {
        request_id: uuid::Uuid::new_v4().to_string(),
        targets: clients
            .iter()
            .map(|client| {
                let status = ownership(app, *client);
                ConfigTakeoverTarget {
                    client: *client,
                    target_id: status.target_id,
                    expected_owner_id: status.owner.map(|owner| owner.instance_id),
                    expected_generation: status.generation,
                }
            })
            .collect(),
    }
}

async fn peer(app: Arc<App>) -> tokio::task::JoinHandle<Result<()>> {
    let endpoint = app.endpoint.read().clone();
    let server_app = app.clone();
    let server_endpoint = endpoint.clone();
    let task = tokio::spawn(async move { crate::rpc::serve_on(server_app, server_endpoint).await });
    let mut params = HelloParams::new("ownership-test", env!("CARGO_PKG_VERSION"));
    params
        .capabilities
        .push(capability::CONFIG_OWNERSHIP.into());
    for _ in 0..100 {
        if let Ok(mut client) = IpcClient::connect(endpoint.clone()).await {
            let hello = client.hello(&params).await.unwrap();
            assert!(
                hello
                    .capabilities
                    .contains(&capability::CONFIG_OWNERSHIP.to_owned())
            );
            assert_eq!(
                hello.instance_id.as_deref(),
                Some(app.instance.instance_id.as_str())
            );
            return task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();
    panic!("temporary ownership peer did not start");
}

async fn stop_peer(task: tokio::task::JoinHandle<Result<()>>) {
    task.abort();
    let _ = task.await;
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn assert_ownership_conflict<T>(result: Result<T>) {
    let error = result
        .err()
        .expect("configuration mutation unexpectedly succeeded");
    assert!(matches!(error, DaemonError::Ownership(_)));
}

fn stage_release(app: &App, params: &ConfigReleaseParams) -> (Target, Record) {
    let target = Target::new(
        params.target.client,
        app.config_path(params.target.client).unwrap(),
    )
    .unwrap();
    let mut guard = target.lock().unwrap();
    let mut record = guard.record().cloned().unwrap();
    app.db
        .begin_operation(
            "release_config",
            params.target.client,
            None,
            &serde_json::to_string(params).unwrap(),
        )
        .unwrap();
    record.pending = Some(Pending {
        request_id: params.request_id.clone(),
        kind: PendingKind::Release,
        requester: Some(params.requester.clone()),
    });
    guard.set_record(record.clone()).unwrap();
    (target, record)
}

fn restore_release_files(app: &App, target: &Target, restore_auth: bool) {
    let official = app.ensure_official_provider(target.client).unwrap();
    let restore = app
        .config_target(&official, ConnectionMode::Direct, None)
        .unwrap();
    config::apply_with_credential(
        &target.config_path,
        config::file_hash(&target.config_path).unwrap().as_deref(),
        &restore,
        None,
    )
    .unwrap();
    if restore_auth && target.client == ClientKind::Codex {
        let auth_path = config::codex_auth_path(&target.config_path).unwrap();
        let current = fs::read_to_string(&auth_path).unwrap();
        let snapshot = app.codex_auth_backup().unwrap().unwrap();
        let restored = config::restore_codex_auth_text(&current, &snapshot).unwrap();
        fs::write(auth_path, restored).unwrap();
    }
}

#[tokio::test]
async fn foreign_switch_preserves_auth_state_and_both_encrypted_backups() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    let official = fixture
        .second
        .ensure_official_provider(ClientKind::Codex)
        .unwrap();
    activate(&fixture.first, &custom).await;
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let owner_before = fixture.owner_record(ClientKind::Codex);
    let state_before = fixture.second.db.client_state(ClientKind::Codex).unwrap();
    let backup_before = fixture
        .first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();

    assert_ownership_conflict(
        fixture
            .second
            .switch_provider(ProviderSwitchParams {
                client: ClientKind::Codex,
                provider_id: official.id,
            })
            .await,
    );

    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert_eq!(fixture.owner_record(ClientKind::Codex), owner_before);
    assert_eq!(
        fixture.second.db.client_state(ClientKind::Codex).unwrap(),
        state_before
    );
    assert!(fixture.second.codex_auth_backup().unwrap().is_none());
    let backup_after = fixture
        .first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(backup_after.nonce, backup_before.nonce);
    assert_eq!(backup_after.ciphertext, backup_before.ciphertext);
    assert_eq!(fixture.second.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn rpc_takeover_restores_official_auth_keeps_oauth_refresh_and_prevents_restart_reclaim() {
    let fixture = Instances::new();
    let server = peer(fixture.first.clone()).await;
    let custom = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    activate(&fixture.first, &custom).await;
    let mut auth = read_json(&fixture.codex_auth());
    auth["tokens"]["access_token"] = json!("refreshed-token");
    fs::write(
        fixture.codex_auth(),
        serde_json::to_vec_pretty(&auth).unwrap(),
    )
    .unwrap();
    let request = takeover(&fixture.second, &[ClientKind::Codex]);
    let request_id = request.request_id.clone();
    let result = fixture
        .second
        .takeover_configuration(request.clone())
        .await
        .unwrap();
    assert_eq!(result.request_id, request_id);
    let generation = ownership(&fixture.second, ClientKind::Codex).generation;
    fixture
        .second
        .takeover_configuration(request)
        .await
        .unwrap();
    assert_eq!(
        ownership(&fixture.second, ClientKind::Codex).generation,
        generation
    );
    let official = fixture
        .second
        .ensure_official_provider(ClientKind::Codex)
        .unwrap();
    activate(&fixture.second, &official).await;
    let restored = read_json(&fixture.codex_auth());
    assert!(restored["auth_mode"] == "chatgpt");
    assert!(restored["tokens"]["access_token"] == "refreshed-token");
    assert!(restored["account_id"] == "user-account");
    assert!(restored.get("OPENAI_API_KEY").is_none());
    assert!(fixture.first.codex_auth_backup().unwrap().is_none());
    assert_eq!(
        fixture
            .first
            .db
            .client_state(ClientKind::Codex)
            .unwrap()
            .active_provider_id
            .as_deref(),
        Some(custom.id.as_str())
    );
    let record = fixture.owner_record(ClientKind::Codex);
    let configured = fs::read(fixture.codex_config()).unwrap();
    let _ = fixture.first.reconcile_client_auth_configuration();
    let _ = fixture.first.reconcile_proxy_configurations();
    fixture.first.recover_operations().unwrap();
    assert_eq!(fixture.owner_record(ClientKind::Codex), record);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), configured);
    assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
    stop_peer(server).await;
}

#[tokio::test]
async fn editing_a_current_provider_blocks_owned_field_drift_before_revision_or_secret_changes() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    activate(&fixture.first, &custom).await;
    let mut auth = read_json(&fixture.codex_auth());
    auth["auth_mode"] = json!("chatgpt");
    fs::write(
        fixture.codex_auth(),
        serde_json::to_vec_pretty(&auth).unwrap(),
    )
    .unwrap();
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let state_before = fixture.first.db.client_state(ClientKind::Codex).unwrap();
    assert_ownership_conflict(
        fixture
            .first
            .edit_provider(ProviderEditParams {
                id: custom.id.clone(),
                expected_revision: custom.revision,
                patch: ProviderPatch {
                    name: Some("Changed".into()),
                    ..ProviderPatch::default()
                },
                secret: SecretInput::Replace("replacement-fixture-key".into()),
                proxy_password: SecretInput::Preserve,
            })
            .await,
    );
    assert_eq!(fixture.first.db.get_provider(&custom.id).unwrap(), custom);
    assert_eq!(
        fixture.first.db.client_state(ClientKind::Codex).unwrap(),
        state_before
    );
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth_before);
    assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn same_owner_allows_token_refresh_and_non_owned_configuration_changes() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    activate(&fixture.first, &custom).await;
    let before = ownership(&fixture.first, ClientKind::Codex);
    let mut auth = read_json(&fixture.codex_auth());
    auth["tokens"]["access_token"] = json!("updated-token");
    auth["user_setting"] = json!("keep");
    fs::write(
        fixture.codex_auth(),
        serde_json::to_vec_pretty(&auth).unwrap(),
    )
    .unwrap();
    let configuration = fs::read_to_string(fixture.codex_config()).unwrap();
    fs::write(
        fixture.codex_config(),
        configuration.replace("user-model", "new-user-model"),
    )
    .unwrap();
    let edited = fixture
        .first
        .edit_provider(ProviderEditParams {
            id: custom.id,
            expected_revision: custom.revision,
            patch: ProviderPatch {
                name: Some("Changed".into()),
                ..ProviderPatch::default()
            },
            secret: SecretInput::Preserve,
            proxy_password: SecretInput::Preserve,
        })
        .await
        .unwrap();
    assert_eq!(edited.revision, custom.revision + 1);
    assert_eq!(
        ownership(&fixture.first, ClientKind::Codex).generation,
        before.generation
    );
    let auth = read_json(&fixture.codex_auth());
    assert!(auth["tokens"]["access_token"] == "updated-token");
    assert!(auth["user_setting"] == "keep");
    assert!(
        fs::read_to_string(fixture.codex_config())
            .unwrap()
            .contains("new-user-model")
    );
}

#[tokio::test]
async fn claude_handoff_restores_model_snapshot_and_preserves_unowned_fields() {
    let fixture = Instances::new();
    let server = peer(fixture.first.clone()).await;
    let mut provider = draft(ClientKind::Claude, "Mapped");
    provider.claude_model_mapping = Some(ClaudeModelMapping {
        enabled: true,
        default_model: Some("mapped-default".into()),
        opus: Some(ModelSlot {
            model: "mapped-opus".into(),
            context_1m: false,
        }),
        ..ClaudeModelMapping::default()
    });
    let custom = add(&fixture.first, provider).await;
    activate(&fixture.first, &custom).await;
    assert!(read_json(&fixture.claude_config())["env"]["ANTHROPIC_MODEL"] == "mapped-default");
    fixture
        .second
        .takeover_configuration(takeover(&fixture.second, &[ClientKind::Claude]))
        .await
        .unwrap();
    let official = fixture
        .second
        .ensure_official_provider(ClientKind::Claude)
        .unwrap();
    activate(&fixture.second, &official).await;
    let restored = read_json(&fixture.claude_config());
    assert!(restored["env"]["ANTHROPIC_MODEL"] == "user-default");
    assert!(restored["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"] == "User Opus");
    assert!(
        restored["env"]
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .is_none()
    );
    assert!(restored["env"].get("ANTHROPIC_API_KEY").is_none());
    assert!(restored["env"].get("ANTHROPIC_BASE_URL").is_none());
    assert!(restored["permissions"]["allow"] == json!(["Read"]));
    assert!(
        fixture
            .first
            .db
            .setting(CLAUDE_MODEL_ENV_BEFORE_KEY)
            .unwrap()
            .is_none()
    );
    stop_peer(server).await;
}

#[tokio::test]
async fn expired_confirmation_and_offline_owner_do_not_release_or_capture_backups() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    activate(&fixture.first, &custom).await;
    let record = fixture.owner_record(ClientKind::Codex);
    let auth = fs::read(fixture.codex_auth()).unwrap();
    let mut expired = takeover(&fixture.second, &[ClientKind::Codex]);
    expired.targets[0].expected_generation += 1;
    assert_ownership_conflict(fixture.second.takeover_configuration(expired).await);
    assert!(
        fixture
            .second
            .takeover_configuration(takeover(&fixture.second, &[ClientKind::Codex]))
            .await
            .is_err()
    );
    assert_eq!(fixture.owner_record(ClientKind::Codex), record);
    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth);
    assert!(fixture.second.codex_auth_backup().unwrap().is_none());
    assert!(ownership(&fixture.first, ClientKind::Codex).owner_is_self);
}

#[tokio::test]
async fn multi_client_settings_conflict_precedes_language_proxy_and_file_mutation() {
    let fixture = Instances::new();
    let codex = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    let chosen = add(&fixture.second, draft(ClientKind::Codex, "Chosen")).await;
    let claude = add(&fixture.second, draft(ClientKind::Claude, "Claude")).await;
    activate(&fixture.first, &codex).await;
    fixture
        .second
        .apply_configuration(&claude, ConnectionMode::Proxy)
        .unwrap();
    fixture
        .second
        .db
        .set_active(ClientKind::Codex, &chosen.id, "conflict")
        .unwrap();
    fixture
        .second
        .db
        .set_mode(ClientKind::Codex, ConnectionMode::Proxy)
        .unwrap();
    let settings_before = fixture.second.settings().unwrap();
    let codex_before = fs::read(fixture.codex_config()).unwrap();
    let claude_before = fs::read(fixture.claude_config()).unwrap();
    let state_before = fixture.second.db.client_state(ClientKind::Claude).unwrap();
    assert_ownership_conflict(
        fixture
            .second
            .update_settings(SettingsPatch {
                language: Some(hsin_core::LANGUAGE_ZH_CN.into()),
                proxy_port: Some(settings_before.proxy_port + 1),
                ..SettingsPatch::default()
            })
            .await,
    );
    assert_eq!(fixture.second.settings().unwrap(), settings_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), codex_before);
    assert_eq!(fs::read(fixture.claude_config()).unwrap(), claude_before);
    assert_eq!(
        fixture.second.db.client_state(ClientKind::Claude).unwrap(),
        state_before
    );
}

#[tokio::test]
async fn image_provider_creation_cannot_mutate_database_before_shared_config_preflight() {
    let fixture = Instances::new();
    let codex = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    let chosen = add(&fixture.second, draft(ClientKind::Codex, "Chosen")).await;
    activate(&fixture.first, &codex).await;
    fixture
        .second
        .db
        .set_active(ClientKind::Codex, &chosen.id, "conflict")
        .unwrap();
    fixture
        .second
        .db
        .set_mode(ClientKind::Codex, ConnectionMode::Proxy)
        .unwrap();
    let providers = fixture.second.db.list_providers(None).unwrap();
    let configured = fs::read(fixture.codex_config()).unwrap();
    let mut image = draft(ClientKind::Codex, "Image");
    image.scope = ProviderScope::ImageOnly;
    image.codex_image = hsin_core::CodexImageConfig {
        enabled: true,
        models: vec!["gpt-image-2".into()],
        preferred_model: Some("gpt-image-2".into()),
    };
    assert_ownership_conflict(
        fixture
            .second
            .add_provider(ProviderAddParams {
                provider: image,
                secret: SecretInput::Replace("image-fixture-key".into()),
                proxy_password: SecretInput::Preserve,
            })
            .await,
    );
    assert_eq!(fixture.second.db.list_providers(None).unwrap(), providers);
    assert!(
        fixture
            .second
            .db
            .image_active_provider_id()
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), configured);
}

#[tokio::test]
async fn legacy_hsin_configuration_cannot_become_a_fresh_original_login_snapshot() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "New")).await;
    fs::write(fixture.codex_config(), "model_provider = \"hsin\"\n[model_providers.hsin]\nbase_url = \"https://legacy.example.test/v1\"\n").unwrap();
    let state_before = fixture.first.db.client_state(ClientKind::Codex).unwrap();
    let configured = fs::read(fixture.codex_config()).unwrap();
    let auth = fs::read(fixture.codex_auth()).unwrap();
    assert_ownership_conflict(
        fixture
            .first
            .switch_provider(ProviderSwitchParams {
                client: ClientKind::Codex,
                provider_id: custom.id,
            })
            .await,
    );
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), configured);
    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth);
    assert_eq!(
        fixture.first.db.client_state(ClientKind::Codex).unwrap(),
        state_before
    );
    assert!(fixture.first.codex_auth_backup().unwrap().is_none());
    assert!(
        ownership(&fixture.first, ClientKind::Codex)
            .takeover_unavailable_reason
            .is_some()
    );
}

#[tokio::test]
async fn handoff_restores_chatgpt_for_direct_api_keys_and_proxy_placeholders() {
    for mode in [ConnectionMode::Direct, ConnectionMode::Proxy] {
        let fixture = Instances::new();
        let server = peer(fixture.first.clone()).await;
        fixture
            .first
            .update_settings(SettingsPatch {
                client_auth: Some(ClientAuthUpdate {
                    client: ClientKind::Codex,
                    disable_custom_auth: true,
                }),
                ..SettingsPatch::default()
            })
            .await
            .unwrap();
        let custom = add(
            &fixture.first,
            draft(ClientKind::Codex, "Custom auth disabled"),
        )
        .await;
        fixture.first.db.set_mode(ClientKind::Codex, mode).unwrap();
        activate(&fixture.first, &custom).await;
        let auth = read_json(&fixture.codex_auth());
        assert!(auth["auth_mode"] == "apikey");
        let expected_key = match mode {
            ConnectionMode::Direct => "fixture-api-key",
            ConnectionMode::Proxy => config::HSIN_MANAGED_KEY,
        };
        assert!(auth["OPENAI_API_KEY"] == expected_key);
        fixture
            .second
            .takeover_configuration(takeover(&fixture.second, &[ClientKind::Codex]))
            .await
            .unwrap();
        assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
        assert!(
            fixture
                .first
                .settings()
                .unwrap()
                .client_auth
                .codex_disable_custom_auth
        );
        stop_peer(server).await;
    }
}

#[tokio::test]
async fn preserving_official_login_excludes_all_auth_fields_from_ownership() {
    let fixture = Instances::new();
    let server = peer(fixture.first.clone()).await;
    fixture
        .first
        .update_settings(SettingsPatch {
            codex_preserve_official_auth: Some(true),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Preserved login")).await;
    activate(&fixture.first, &custom).await;
    assert!(fixture.first.codex_auth_backup().unwrap().is_none());
    let refreshed = "{\"auth_mode\":\"chatgpt\",\"OPENAI_API_KEY\":null,\"tokens\":{\"access_token\":\"new-login\"}}\n";
    fs::write(fixture.codex_auth(), refreshed).unwrap();
    fixture
        .second
        .takeover_configuration(takeover(&fixture.second, &[ClientKind::Codex]))
        .await
        .unwrap();
    assert!(fs::read_to_string(fixture.codex_auth()).unwrap() == refreshed);
    assert!(
        fixture
            .first
            .settings()
            .unwrap()
            .client_auth
            .codex_preserve_official_auth
    );
    stop_peer(server).await;
}

#[tokio::test]
async fn simultaneous_opposite_handoffs_do_not_hold_mutation_locks_across_peer_rpc() {
    let fixture = Instances::new();
    let first_server = peer(fixture.first.clone()).await;
    let second_server = peer(fixture.second.clone()).await;
    let codex = add(&fixture.first, draft(ClientKind::Codex, "Codex owner")).await;
    let claude = add(&fixture.second, draft(ClientKind::Claude, "Claude owner")).await;
    activate(&fixture.first, &codex).await;
    activate(&fixture.second, &claude).await;
    let first_request = takeover(&fixture.first, &[ClientKind::Claude]);
    let second_request = takeover(&fixture.second, &[ClientKind::Codex]);
    let (first_result, second_result) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                fixture.first.takeover_configuration(first_request),
                fixture.second.takeover_configuration(second_request)
            )
        })
        .await
        .expect("opposite configuration handoffs deadlocked");
    first_result.unwrap();
    second_result.unwrap();
    assert!(ownership(&fixture.first, ClientKind::Claude).owner_is_self);
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
    assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
    assert!(
        read_json(&fixture.claude_config())["env"]
            .get("ANTHROPIC_API_KEY")
            .is_none()
    );
    stop_peer(first_server).await;
    stop_peer(second_server).await;
}

#[tokio::test]
async fn a_persisted_release_reservation_can_be_retried_after_the_old_owner_goes_offline() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    activate(&fixture.first, &custom).await;
    let request = takeover(&fixture.second, &[ClientKind::Codex]);
    let release = ConfigReleaseParams {
        request_id: request.request_id.clone(),
        target: request.targets[0].clone(),
        requester: fixture.second.instance.clone(),
    };
    let first = fixture
        .first
        .release_configuration(release.clone())
        .await
        .unwrap();
    let auth_after_release = fs::read(fixture.codex_auth()).unwrap();
    let reserved_record = fixture.owner_record(ClientKind::Codex);
    let second = fixture.first.release_configuration(release).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(fixture.owner_record(ClientKind::Codex), reserved_record);
    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth_after_release);
    assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
    assert!(fixture.first.codex_auth_backup().unwrap().is_none());
    // No RPC listener is running. The completed release is sufficient to retry the receiver phase.
    fixture
        .second
        .takeover_configuration(request)
        .await
        .unwrap();
    let acquired = ownership(&fixture.second, ClientKind::Codex);
    assert!(acquired.owner_is_self);
    assert_eq!(acquired.generation, first.generation + 1);
    assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
}

#[tokio::test]
async fn replaying_an_old_release_receipt_preserves_a_new_codex_and_claude_lease() {
    let fixture = Instances::new();
    let first = &fixture.first;
    let first_server = peer(fixture.first.clone()).await;
    let second_server = peer(fixture.second.clone()).await;
    let mut mapped = draft(ClientKind::Claude, "First Claude");
    mapped.claude_model_mapping = Some(ClaudeModelMapping {
        enabled: true,
        default_model: Some("mapped-default".into()),
        ..ClaudeModelMapping::default()
    });
    let first_codex = add(&fixture.first, draft(ClientKind::Codex, "First Codex")).await;
    let first_claude = add(&fixture.first, mapped).await;
    activate(&fixture.first, &first_codex).await;
    activate(&fixture.first, &first_claude).await;
    let request = takeover(&fixture.second, &[ClientKind::Codex, ClientKind::Claude]);
    let old_releases: Vec<_> = request
        .targets
        .iter()
        .map(|target| ConfigReleaseParams {
            request_id: request.request_id.clone(),
            target: target.clone(),
            requester: fixture.second.instance.clone(),
        })
        .collect();
    fixture
        .second
        .takeover_configuration(request)
        .await
        .unwrap();
    let second_codex = add(&fixture.second, draft(ClientKind::Codex, "Second Codex")).await;
    let second_claude = add(&fixture.second, draft(ClientKind::Claude, "Second Claude")).await;
    activate(&fixture.second, &second_codex).await;
    activate(&fixture.second, &second_claude).await;
    fixture
        .first
        .takeover_configuration(takeover(
            &fixture.first,
            &[ClientKind::Codex, ClientKind::Claude],
        ))
        .await
        .unwrap();
    activate(&fixture.first, &first_codex).await;
    activate(&fixture.first, &first_claude).await;
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let codex_before = fs::read(fixture.codex_config()).unwrap();
    let claude_before = fs::read(fixture.claude_config()).unwrap();
    let models_before = first.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY).unwrap();
    assert!(models_before.is_some());
    let backup_before = first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();
    for release in old_releases {
        let lease_key = format!("config_lease:{}", release.target.target_id);
        let lease_before = first.db.setting(&lease_key).unwrap();
        let state_before = first.db.client_state(release.target.client).unwrap();
        let sidecar_before = fixture.owner_record(release.target.client);
        let current = ownership(&fixture.first, release.target.client);
        assert!(current.generation > release.target.expected_generation);
        assert!(current.owner_is_self);
        first.release_configuration(release.clone()).await.unwrap();
        assert_eq!(first.db.setting(&lease_key).unwrap(), lease_before);
        assert_eq!(
            first.db.client_state(release.target.client).unwrap(),
            state_before
        );
        assert_eq!(fixture.owner_record(release.target.client), sidecar_before);
    }
    assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), codex_before);
    assert!(fs::read(fixture.claude_config()).unwrap() == claude_before);
    assert_eq!(
        first.db.setting(CLAUDE_MODEL_ENV_BEFORE_KEY).unwrap(),
        models_before
    );
    let retained = first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();
    assert!(retained.ciphertext == backup_before.ciphertext);
    stop_peer(first_server).await;
    stop_peer(second_server).await;
}

#[tokio::test]
async fn partial_multi_target_takeover_retries_only_the_unfinished_target() {
    let fixture = Instances::new();
    let first_server = peer(fixture.first.clone()).await;
    let third = App::open_inner(
        &Paths::for_home(fixture.temporary.0.join("third")),
        Arc::new(MemoryStore::default()),
        Some(&fixture.temporary.0.join("codex")),
        Some(&fixture.temporary.0.join("claude")),
    )
    .unwrap();
    #[cfg(unix)]
    let endpoint = IpcEndpoint::filesystem(fixture.temporary.0.join("third.sock"));
    #[cfg(not(unix))]
    let endpoint = IpcEndpoint::namespaced(format!("hsown-third-{}", uuid::Uuid::new_v4()));
    *third.endpoint.write() = endpoint;
    let codex = add(&fixture.first, draft(ClientKind::Codex, "Codex owner")).await;
    let claude = add(&third, draft(ClientKind::Claude, "Offline Claude owner")).await;
    activate(&fixture.first, &codex).await;
    activate(&third, &claude).await;
    let request = takeover(&fixture.second, &[ClientKind::Codex, ClientKind::Claude]);
    assert!(
        fixture
            .second
            .takeover_configuration(request.clone())
            .await
            .is_err()
    );
    let completed_generation = ownership(&fixture.second, ClientKind::Codex).generation;
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
    assert!(ownership(&third, ClientKind::Claude).owner_is_self);
    assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
    let third_server = peer(third.clone()).await;
    fixture
        .second
        .takeover_configuration(request)
        .await
        .unwrap();
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
    assert!(ownership(&fixture.second, ClientKind::Claude).owner_is_self);
    assert_eq!(
        ownership(&fixture.second, ClientKind::Codex).generation,
        completed_generation
    );
    stop_peer(first_server).await;
    stop_peer(third_server).await;
}

#[tokio::test]
async fn restart_recovers_release_after_config_or_auth_write_before_sidecar_handoff() {
    for restore_auth in [false, true] {
        let fixture = Instances::new();
        let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
        activate(&fixture.first, &custom).await;
        let request = takeover(&fixture.second, &[ClientKind::Codex]);
        let release = ConfigReleaseParams {
            request_id: request.request_id.clone(),
            target: request.targets[0].clone(),
            requester: fixture.second.instance.clone(),
        };
        let (target, original_record) = stage_release(&fixture.first, &release);
        restore_release_files(&fixture.first, &target, restore_auth);
        let restored_config = fs::read(&target.config_path).unwrap();
        let restarted = fixture.reopen_first();
        restarted.recover_operations().unwrap();
        assert_eq!(restarted.db.pending_operations().unwrap().len(), 0);
        assert!(restarted.codex_auth_backup().unwrap().is_none());
        assert_eq!(fs::read(&target.config_path).unwrap(), restored_config);
        assert!(read_json(&fixture.codex_auth())["auth_mode"] == "chatgpt");
        let reserved = target.read_record().unwrap().unwrap();
        assert!(reserved.owner.is_none());
        assert_eq!(reserved.generation, original_record.generation + 1);
        assert_eq!(
            reserved.pending.as_ref().unwrap().kind,
            PendingKind::Takeover
        );
        let after_recovery = fixture.owner_record(ClientKind::Codex);
        restarted.recover_operations().unwrap();
        assert_eq!(fixture.owner_record(ClientKind::Codex), after_recovery);
        fixture
            .second
            .takeover_configuration(request)
            .await
            .unwrap();
        assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
    }
}

#[tokio::test]
async fn restart_finishes_cleanup_when_sidecar_handoff_precedes_backup_and_journal_cleanup() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    activate(&fixture.first, &custom).await;
    let request = takeover(&fixture.second, &[ClientKind::Codex]);
    let release = ConfigReleaseParams {
        request_id: request.request_id.clone(),
        target: request.targets[0].clone(),
        requester: fixture.second.instance.clone(),
    };
    let (target, original_record) = stage_release(&fixture.first, &release);
    restore_release_files(&fixture.first, &target, true);
    let scope = ManagedScope::default();
    let mut reserved = Record::unclaimed(
        original_record.generation + 1,
        scope.clone(),
        config::ownership_fingerprints(ClientKind::Codex, &target.config_path, &scope).unwrap(),
    );
    reserved.pending = Some(Pending {
        request_id: request.request_id.clone(),
        kind: PendingKind::Takeover,
        requester: Some(fixture.second.instance.clone()),
    });
    target.lock().unwrap().set_record(reserved).unwrap();
    assert!(fixture.first.codex_auth_backup().unwrap().is_some());
    assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 1);
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let sidecar_before = fixture.owner_record(ClientKind::Codex);
    let restarted = fixture.reopen_first();
    restarted.recover_operations().unwrap();
    assert_eq!(restarted.db.pending_operations().unwrap().len(), 0);
    assert!(restarted.codex_auth_backup().unwrap().is_none());
    assert!(
        restarted
            .db
            .setting(&format!("config_lease:{}", target.id))
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth_before);
    assert_eq!(fixture.owner_record(ClientKind::Codex), sidecar_before);
    restarted.recover_operations().unwrap();
    fixture
        .second
        .takeover_configuration(request)
        .await
        .unwrap();
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
}

#[tokio::test]
async fn external_relogin_or_api_key_change_blocks_release_without_overwriting_new_auth() {
    for key in ["auth_mode", "OPENAI_API_KEY"] {
        let fixture = Instances::new();
        let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
        activate(&fixture.first, &custom).await;
        let request = takeover(&fixture.second, &[ClientKind::Codex]);
        let release = ConfigReleaseParams {
            request_id: request.request_id,
            target: request.targets[0].clone(),
            requester: fixture.second.instance.clone(),
        };
        let mut auth = read_json(&fixture.codex_auth());
        auth[key] = match key {
            "auth_mode" => json!("chatgpt"),
            _ => json!("externally-changed-fixture-key"),
        };
        fs::write(
            fixture.codex_auth(),
            serde_json::to_vec_pretty(&auth).unwrap(),
        )
        .unwrap();
        let auth_before = fs::read(fixture.codex_auth()).unwrap();
        let config_before = fs::read(fixture.codex_config()).unwrap();
        let record_before = fixture.owner_record(ClientKind::Codex);
        assert_ownership_conflict(fixture.first.release_configuration(release).await);
        assert_eq!(fs::read(fixture.codex_auth()).unwrap(), auth_before);
        assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
        assert_eq!(fixture.owner_record(ClientKind::Codex), record_before);
        assert!(fixture.first.codex_auth_backup().unwrap().is_some());
        assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn an_unreadable_auth_backup_blocks_direct_and_rpc_release_before_any_file_changes() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    activate(&fixture.first, &custom).await;
    let request = takeover(&fixture.second, &[ClientKind::Codex]);
    let release = ConfigReleaseParams {
        request_id: request.request_id.clone(),
        target: request.targets[0].clone(),
        requester: fixture.second.instance.clone(),
    };
    let mut encrypted = fixture
        .first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();
    encrypted.ciphertext[0] ^= 1;
    fixture.first.db.put_protected_value(&encrypted).unwrap();
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let sidecar_before = fixture.owner_record(ClientKind::Codex);
    let state_before = fixture.first.db.client_state(ClientKind::Codex).unwrap();
    let settings_before = fixture.first.settings().unwrap();
    let providers_before = fixture.first.db.list_providers(None).unwrap();
    let expected = "Codex authentication backup could not be decrypted; configuration files were left unchanged";
    let direct = fixture.first.release_configuration(release).await;
    assert!(matches!(direct, Err(DaemonError::Config(message)) if message == expected));
    let server = peer(fixture.first.clone()).await;
    let rpc = fixture.second.takeover_configuration(request).await;
    stop_peer(server).await;
    assert!(
        matches!(rpc, Err(DaemonError::Config(message)) if message == format!("codex: {expected}"))
    );
    assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert_eq!(fixture.owner_record(ClientKind::Codex), sidecar_before);
    assert_eq!(
        fixture.first.db.client_state(ClientKind::Codex).unwrap(),
        state_before
    );
    assert_eq!(fixture.first.settings().unwrap(), settings_before);
    assert_eq!(
        fixture.first.db.list_providers(None).unwrap(),
        providers_before
    );
    let retained = fixture
        .first
        .db
        .protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap()
        .unwrap();
    assert!(retained.ciphertext == encrypted.ciphertext);
    assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn foreign_claude_ownership_does_not_block_official_auth_preference_without_file_changes() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Claude, "Owner")).await;
    let official = fixture
        .second
        .ensure_official_provider(ClientKind::Claude)
        .unwrap();
    activate(&fixture.first, &custom).await;
    fixture
        .second
        .db
        .set_active(ClientKind::Claude, &official.id, "conflict")
        .unwrap();
    let settings_before = fs::read(fixture.claude_config()).unwrap();
    let sidecar_before = fixture.owner_record(ClientKind::Claude);
    let changed = fixture
        .second
        .update_settings(SettingsPatch {
            client_auth: Some(ClientAuthUpdate {
                client: ClientKind::Claude,
                disable_custom_auth: true,
            }),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    assert!(changed.client_auth.claude_disable_custom_auth);
    assert_eq!(fs::read(fixture.claude_config()).unwrap(), settings_before);
    assert_eq!(fixture.owner_record(ClientKind::Claude), sidecar_before);
    assert!(ownership(&fixture.first, ClientKind::Claude).owner_is_self);
    assert_eq!(fixture.second.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn foreign_claude_ownership_does_not_block_names_preference_for_unmapped_selection() {
    let fixture = Instances::new();
    let owner = add(&fixture.first, draft(ClientKind::Claude, "Owner")).await;
    let chosen = add(
        &fixture.second,
        draft(ClientKind::Claude, "Unmapped selection"),
    )
    .await;
    activate(&fixture.first, &owner).await;
    fixture
        .second
        .db
        .set_active(ClientKind::Claude, &chosen.id, "conflict")
        .unwrap();
    let settings_before = fs::read(fixture.claude_config()).unwrap();
    let sidecar_before = fixture.owner_record(ClientKind::Claude);
    let changed = fixture
        .second
        .update_settings(SettingsPatch {
            claude_model_names_enabled: Some(false),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    assert!(!changed.claude_model_names_enabled);
    assert_eq!(fs::read(fixture.claude_config()).unwrap(), settings_before);
    assert_eq!(fixture.owner_record(ClientKind::Claude), sidecar_before);
    assert!(ownership(&fixture.first, ClientKind::Claude).owner_is_self);
    assert_eq!(fixture.second.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn a_new_transaction_requires_current_confirmation_to_reuse_a_release_reservation() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Owner")).await;
    activate(&fixture.first, &custom).await;
    let original = takeover(&fixture.second, &[ClientKind::Codex]);
    fixture
        .first
        .release_configuration(ConfigReleaseParams {
            request_id: original.request_id.clone(),
            target: original.targets[0].clone(),
            requester: fixture.second.instance.clone(),
        })
        .await
        .unwrap();
    let record_before = fixture.owner_record(ClientKind::Codex);
    let mut stale = original.clone();
    stale.request_id = uuid::Uuid::new_v4().to_string();
    assert_ownership_conflict(fixture.second.takeover_configuration(stale).await);
    assert_eq!(fixture.owner_record(ClientKind::Codex), record_before);
    let replacement = takeover(&fixture.second, &[ClientKind::Codex]);
    assert_ne!(replacement.request_id, original.request_id);
    fixture
        .second
        .takeover_configuration(replacement)
        .await
        .unwrap();
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
}

#[tokio::test]
async fn a_native_relogin_quarantines_polluted_legacy_backup_and_never_replays_its_operation() {
    let fixture = Instances::new();
    let custom = add(&fixture.first, draft(ClientKind::Codex, "Legacy owner")).await;
    activate(&fixture.first, &custom).await;
    let polluted = config::CodexAuthSnapshot {
        auth_path: fixture.codex_auth().to_string_lossy().into_owned(),
        file_existed: true,
        auth_mode: Some("apikey".into()),
        openai_api_key: Some("polluted-fixture-key".into()),
        lease: None,
    };
    let encrypted = fixture
        .first
        .crypto
        .encrypt_protected(
            CODEX_AUTH_BACKUP_KEY,
            &serde_json::to_vec(&polluted).unwrap(),
        )
        .unwrap();
    fixture.first.db.put_protected_value(&encrypted).unwrap();
    let mut legacy_target = fixture
        .first
        .config_target(&custom, ConnectionMode::Direct, None)
        .unwrap();
    legacy_target.ownership_lease = None;
    let operation = fixture
        .first
        .db
        .begin_operation(
            "apply_config",
            ClientKind::Codex,
            None,
            &serde_json::to_string(&legacy_target).unwrap(),
        )
        .unwrap();
    fs::remove_file(fixture.temporary.0.join("codex/.hsin-config-owner.json")).unwrap();
    fs::write(
        fixture.codex_config(),
        "model_provider = \"openai\"\nmodel = \"fresh-user-model\"\n",
    )
    .unwrap();
    fs::write(
        fixture.codex_auth(),
        "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"fresh-official-login\"}}\n",
    )
    .unwrap();
    activate(&fixture.first, &custom).await;
    let baseline = fixture.first.codex_auth_backup().unwrap().unwrap();
    assert!(baseline.auth_mode.as_deref() == Some("chatgpt"));
    assert!(baseline.openai_api_key.is_none());
    assert!(baseline.lease.is_some());
    let quarantined = fixture
        .first
        .db
        .all_protected_values()
        .unwrap()
        .into_iter()
        .filter(|value| value.key.starts_with("quarantined_config_auth:"))
        .collect::<Vec<_>>();
    assert_eq!(quarantined.len(), 1);
    let envelope = fixture
        .first
        .crypto
        .decrypt_protected(&quarantined[0])
        .unwrap();
    let (key, version, nonce, ciphertext): (String, u32, Vec<u8>, Vec<u8>) =
        serde_json::from_slice(&envelope).unwrap();
    assert_eq!(key, encrypted.key);
    assert_eq!(version, encrypted.key_version);
    assert_eq!(nonce, encrypted.nonce);
    assert_eq!(ciphertext, encrypted.ciphertext);
    let state: String = fixture
        .first
        .db
        .connection
        .lock()
        .query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&operation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "quarantined");
    fixture.first.recover_operations().unwrap();
    let official = fixture
        .first
        .ensure_official_provider(ClientKind::Codex)
        .unwrap();
    activate(&fixture.first, &official).await;
    let restored = read_json(&fixture.codex_auth());
    assert!(restored["auth_mode"] == "chatgpt");
    assert!(restored["tokens"]["access_token"] == "fresh-official-login");
    assert!(restored.get("OPENAI_API_KEY").is_none());
    assert!(fixture.first.codex_auth_backup().unwrap().is_none());
    assert!(
        fs::read_to_string(fixture.codex_config())
            .unwrap()
            .contains("fresh-user-model")
    );
}

#[tokio::test]
async fn a_missing_required_auth_backup_blocks_custom_official_and_recovery_before_mutation() {
    let fixture = Instances::new();
    let first = add(&fixture.first, draft(ClientKind::Codex, "First")).await;
    let second = add(&fixture.first, draft(ClientKind::Codex, "Second")).await;
    let official = fixture
        .first
        .ensure_official_provider(ClientKind::Codex)
        .unwrap();
    activate(&fixture.first, &first).await;
    let target = fixture
        .first
        .config_target(&second, ConnectionMode::Direct, None)
        .unwrap();
    let target_json = serde_json::to_string(&target).unwrap();
    let config_hash = config::file_hash(&fixture.codex_config()).unwrap();
    let ownership = ownership(&fixture.first, ClientKind::Codex);
    let marker_key = format!(
        "config_auth_backup_required:{}:{}",
        ownership.target_id, ownership.generation
    );
    let marker_before = fixture.first.db.setting(&marker_key).unwrap();
    assert_eq!(marker_before.as_deref(), Some("true"));
    // Simulate losing the encrypted row while retaining its durable required marker.
    fixture
        .first
        .db
        .delete_protected_value(CODEX_AUTH_BACKUP_KEY)
        .unwrap();
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let sidecar_before = fixture.owner_record(ClientKind::Codex);
    let state_before = fixture.first.db.client_state(ClientKind::Codex).unwrap();
    let settings_before = fixture.first.settings().unwrap();
    let providers_before = fixture.first.db.list_providers(None).unwrap();
    for provider_id in [second.id, official.id] {
        assert!(matches!(
            fixture
                .first
                .switch_provider(ProviderSwitchParams {
                    client: ClientKind::Codex,
                    provider_id,
                })
                .await,
            Err(DaemonError::Conflict(_))
        ));
        assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
        assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
        assert_eq!(fixture.owner_record(ClientKind::Codex), sidecar_before);
        assert_eq!(
            fixture.first.db.client_state(ClientKind::Codex).unwrap(),
            state_before
        );
        assert_eq!(fixture.first.settings().unwrap(), settings_before);
        assert_eq!(
            fixture.first.db.setting(&marker_key).unwrap(),
            marker_before
        );
        assert!(
            fixture
                .first
                .db
                .protected_value(CODEX_AUTH_BACKUP_KEY)
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
    }
    assert!(matches!(
        fixture
            .first
            .recover_operation(ClientKind::Codex, config_hash.as_deref(), &target_json),
        Err(DaemonError::Conflict(_))
    ));
    assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert_eq!(fixture.owner_record(ClientKind::Codex), sidecar_before);
    assert_eq!(
        fixture.first.db.client_state(ClientKind::Codex).unwrap(),
        state_before
    );
    assert_eq!(fixture.first.settings().unwrap(), settings_before);
    assert_eq!(
        fixture.first.db.list_providers(None).unwrap(),
        providers_before
    );
    assert_eq!(
        fixture.first.db.setting(&marker_key).unwrap(),
        marker_before
    );
    assert!(
        fixture
            .first
            .db
            .protected_value(CODEX_AUTH_BACKUP_KEY)
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.first.db.pending_operations().unwrap().len(), 0);
}

#[tokio::test]
async fn a_fresh_confirmation_finishes_a_claim_after_sidecar_precedes_local_receipt() {
    let fixture = Instances::new();
    let previous = add(&fixture.first, draft(ClientKind::Codex, "Previous owner")).await;
    let chosen = add(
        &fixture.second,
        draft(ClientKind::Codex, "Receiver selection"),
    )
    .await;
    activate(&fixture.first, &previous).await;
    fixture
        .second
        .db
        .set_active(ClientKind::Codex, &chosen.id, "conflict")
        .unwrap();
    fixture
        .second
        .db
        .set_mode(ClientKind::Codex, ConnectionMode::Proxy)
        .unwrap();
    let original = takeover(&fixture.second, &[ClientKind::Codex]);
    fixture
        .first
        .release_configuration(ConfigReleaseParams {
            request_id: original.request_id.clone(),
            target: original.targets[0].clone(),
            requester: fixture.second.instance.clone(),
        })
        .await
        .unwrap();
    let target = Target::new(ClientKind::Codex, fixture.codex_config()).unwrap();
    let mut interrupted = target.read_record().unwrap().unwrap();
    interrupted.generation += 1;
    interrupted.owner = Some(fixture.second.instance.clone());
    interrupted.endpoint = Some(fixture.second.endpoint.read().clone());
    target
        .lock()
        .unwrap()
        .set_record(interrupted.clone())
        .unwrap();
    let receipt_key = format!("config_lease:{}", target.id);
    assert!(fixture.second.db.setting(&receipt_key).unwrap().is_none());
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let auth_before = fs::read(fixture.codex_auth()).unwrap();
    let record_before = fixture.owner_record(ClientKind::Codex);
    let _ = fixture.second.reconcile_proxy_configurations();
    let _ = fixture.second.reconcile_client_auth_configuration();
    assert_eq!(fixture.owner_record(ClientKind::Codex), record_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
    assert!(fixture.second.db.setting(&receipt_key).unwrap().is_none());
    let confirmed = takeover(&fixture.second, &[ClientKind::Codex]);
    assert_ne!(confirmed.request_id, original.request_id);
    assert_eq!(
        confirmed.targets[0].expected_owner_id.as_deref(),
        Some(fixture.second.instance.instance_id.as_str())
    );
    fixture
        .second
        .takeover_configuration(confirmed)
        .await
        .unwrap();
    let completed = target.read_record().unwrap().unwrap();
    assert_eq!(completed.generation, interrupted.generation);
    assert!(completed.pending.is_none());
    assert_eq!(
        fixture.second.db.setting(&receipt_key).unwrap().as_deref(),
        Some(interrupted.generation.to_string().as_str())
    );
    assert_eq!(
        fixture
            .second
            .db
            .client_state(ClientKind::Codex)
            .unwrap()
            .config_status,
        hsin_core::ConfigStatus::Unmanaged
    );
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert!(fs::read(fixture.codex_auth()).unwrap() == auth_before);
    assert!(ownership(&fixture.second, ClientKind::Codex).owner_is_self);
}

#[tokio::test]
async fn a_nested_transaction_cannot_reuse_a_shared_directory_lock_for_a_foreign_target() {
    let fixture = Instances::new();
    let claude_path = fixture.temporary.0.join("codex/settings.json");
    fs::rename(fixture.claude_config(), &claude_path).unwrap();
    for app in [&fixture.first, &fixture.second] {
        app.config_paths
            .write()
            .insert(ClientKind::Claude, claude_path.clone());
    }
    let codex = add(&fixture.first, draft(ClientKind::Codex, "Codex owner")).await;
    let claude = add(&fixture.second, draft(ClientKind::Claude, "Claude owner")).await;
    activate(&fixture.first, &codex).await;
    activate(&fixture.second, &claude).await;
    let _outer = fixture
        .first
        .begin_config_transaction(&[ClientKind::Codex], false)
        .unwrap();
    let record_before = fixture.owner_record(ClientKind::Codex);
    let config_before = fs::read(fixture.codex_config()).unwrap();
    let claude_before = fs::read(&claude_path).unwrap();
    let first_state = fixture.first.db.client_state(ClientKind::Claude).unwrap();
    let second_state = fixture.second.db.client_state(ClientKind::Claude).unwrap();
    assert_ownership_conflict(
        fixture
            .first
            .begin_config_transaction(&[ClientKind::Claude], false),
    );
    assert_eq!(fixture.owner_record(ClientKind::Codex), record_before);
    assert_eq!(fs::read(fixture.codex_config()).unwrap(), config_before);
    assert_eq!(fs::read(&claude_path).unwrap(), claude_before);
    assert_eq!(
        fixture.first.db.client_state(ClientKind::Claude).unwrap(),
        first_state
    );
    assert_eq!(
        fixture.second.db.client_state(ClientKind::Claude).unwrap(),
        second_state
    );
    assert!(ownership(&fixture.second, ClientKind::Claude).owner_is_self);
}
