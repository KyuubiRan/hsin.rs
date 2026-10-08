// Test credentials are synthetic; boolean assertions keep tokens out of diagnostics.
#![allow(clippy::manual_assert_eq)]
use super::*;
use crate::app::KeyStore;
use crate::native_auth::NativeIdentity;
use crate::paths::Paths;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hsin_core::{
    AuthScheme, ConnectionMode, ProviderRemoveParams, ProviderSwitchParams, SettingsPatch,
};
use parking_lot::Mutex as ParkingMutex;
use serde_json::json;
use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

#[derive(Default)]
struct MemoryKeys(ParkingMutex<HashMap<u32, String>>);
impl KeyStore for MemoryKeys {
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

struct Fixture {
    app: Arc<App>,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("hsacct-{}", uuid::Uuid::new_v4()));
        let app = App::open_with_store(
            &Paths::for_home(root.join("instance")),
            Arc::new(MemoryKeys::default()),
        )
        .unwrap();
        for client in ClientKind::ALL {
            let home = root.join(client.as_str());
            fs::create_dir_all(&home).unwrap();
            let path = home.join(if client == ClientKind::Codex {
                "config.toml"
            } else {
                "settings.json"
            });
            fs::write(
                &path,
                if client == ClientKind::Codex {
                    "# user setting\nmodel_provider = \"openai\"\n"
                } else {
                    "{\"permissions\":{\"allow\":[\"Read\"]}}"
                },
            )
            .unwrap();
            app.config_paths.write().insert(client, path);
        }
        Self { app, root }
    }
    fn write(&self, snapshot: &NativeAuthSnapshot) {
        let store = self.app.native_auth_store(snapshot.client).unwrap();
        store
            .apply(snapshot, &store.fingerprint().unwrap())
            .unwrap();
    }
    fn current(&self, client: ClientKind) -> NativeAuthSnapshot {
        self.app
            .native_auth_store(client)
            .unwrap()
            .capture()
            .unwrap()
    }
    async fn save(&self, snapshot: &NativeAuthSnapshot) -> Provider {
        self.app
            .save_official_account(snapshot, false)
            .await
            .unwrap()
    }
    async fn switch(&self, provider: &Provider) {
        self.app
            .switch_provider(ProviderSwitchParams {
                client: provider.client,
                provider_id: provider.id.clone(),
            })
            .await
            .unwrap();
    }
    fn api(&self, client: ClientKind) -> Provider {
        let mut provider = self.app.ensure_official_provider(client).unwrap();
        provider.id = uuid::Uuid::new_v4().to_string();
        provider.name = "API".into();
        provider.official = false;
        provider.auth_scheme = if client == ClientKind::Codex {
            AuthScheme::Bearer
        } else {
            AuthScheme::XApiKey
        };
        provider.official_account = None;
        provider.base_url = "https://provider.example.test/v1".into();
        let encrypted = self
            .app
            .crypto
            .encrypt_for(&provider, "test-api-key")
            .unwrap();
        self.app
            .db
            .insert_provider(&provider, Some(&encrypted), None)
            .unwrap();
        self.app.db.get_provider(&provider.id).unwrap()
    }
    fn other(&self) -> Arc<App> {
        let mut app = App::open_with_store(
            &Paths::for_home(self.root.join("other")),
            Arc::new(MemoryKeys::default()),
        )
        .unwrap();
        let app_mut = Arc::get_mut(&mut app).unwrap();
        app_mut.native_credentials = self.app.native_credentials.clone();
        *app_mut.config_paths.write() = self.app.config_paths.read().clone();
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn snapshot(client: ClientKind, user: &str, org: &str, refresh: &str) -> NativeAuthSnapshot {
    let auth = match client {
        ClientKind::Codex => {
            let claims = json!({"sub":user,"name":"Original","email":"same@example.test","https://api.openai.com/auth":{"chatgpt_user_id":user,"chatgpt_account_id":org}});
            let jwt = format!(
                "e30.{}.sig",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            );
            json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":format!("access-{user}-{refresh}"),"refresh_token":refresh,"account_id":org},"last_refresh":"2026-10-08T00:00:00Z"})
        }
        ClientKind::Claude => {
            json!({"claudeAiOauth":{"accessToken":format!("access-{user}-{refresh}"),"refreshToken":refresh,"expiresAt":1_999_999_999_999_u64,"scopes":["user:inference"]}})
        }
    };
    let metadata = if client == ClientKind::Claude {
        json!({"oauthAccount":{"accountUuid":user,"organizationUuid":org,"emailAddress":"same@example.test","displayName":"Original"}})
    } else {
        json!({})
    };
    NativeAuthSnapshot {
        client,
        identity: Some(NativeIdentity {
            account_id: user.into(),
            organization_id: org.into(),
            email: Some("same@example.test".into()),
            native_name: Some("Original".into()),
        }),
        auth,
        metadata,
    }
}

fn refresh(snapshot: &mut NativeAuthSnapshot, value: &str) {
    if snapshot.client == ClientKind::Codex {
        snapshot.auth["tokens"]["refresh_token"] = json!(value);
    } else {
        snapshot.auth["claudeAiOauth"]["refreshToken"] = json!(value);
    }
}

#[tokio::test]
async fn automatic_capture_waits_for_configuration_and_native_locks() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        fixture.write(&snapshot(client, "native", "org", "original"));
        let target =
            crate::ownership::Target::new(client, fixture.app.config_path(client).unwrap())
                .unwrap();
        let target_guard = target.lock().unwrap();
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        assert!(
            fixture
                .app
                .db
                .find_official_account(client, "native", "org")
                .unwrap()
                .is_none()
        );
        drop(target_guard);
        let store = fixture.app.native_auth_store(client).unwrap();
        let native_guard = store.lock_for_switch().unwrap();
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        assert!(
            fixture
                .app
                .db
                .find_official_account(client, "native", "org")
                .unwrap()
                .is_none()
        );
        drop(native_guard);
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        assert!(
            fixture
                .app
                .db
                .find_official_account(client, "native", "org")
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn codex_profile_refresh_restores_latest_native_tokens_and_summary() {
    let fixture = Fixture::new();
    let native = snapshot(ClientKind::Codex, "native", "org", "original");
    fixture.write(&native);
    fixture
        .app
        .capture_existing_official_accounts()
        .await
        .unwrap();
    let account = fixture.save(&native).await;
    fixture.switch(&account).await;
    let mut updated = snapshot(ClientKind::Codex, "native", "org", "refreshed");
    let claims = json!({"sub":"native","name":"Renamed","email":"new@example.test","https://api.openai.com/auth":{"chatgpt_user_id":"native","chatgpt_account_id":"org"}});
    updated.auth["tokens"]["id_token"] = json!(format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    ));
    updated.identity.as_mut().unwrap().email = Some("new@example.test".into());
    updated.identity.as_mut().unwrap().native_name = Some("Renamed".into());
    fixture.write(&updated);
    let other = fixture
        .save(&snapshot(ClientKind::Codex, "other", "org", "other"))
        .await;
    fixture.switch(&other).await;
    let original = fixture
        .app
        .ensure_official_provider(ClientKind::Codex)
        .unwrap();
    fixture.switch(&original).await;
    let restored = fixture.current(ClientKind::Codex);
    assert!(restored.auth == updated.auth);
    assert_eq!(
        restored.identity.as_ref().unwrap().email.as_deref(),
        Some("new@example.test")
    );
    fixture
        .app
        .capture_existing_official_accounts()
        .await
        .unwrap();
    let provider = fixture
        .app
        .list_providers(&hsin_core::ProviderListParams {
            client: Some(ClientKind::Codex),
        })
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == original.id)
        .unwrap();
    assert_eq!(
        provider.official_account.unwrap().email.as_deref(),
        Some("new@example.test")
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn external_codex_fallback_login_blocks_switch_and_release() {
    let fixture = Fixture::new();
    let path = fixture.app.config_path(ClientKind::Codex).unwrap();
    fs::write(
        &path,
        "model_provider = \"openai\"\ncli_auth_credentials_store = \"auto\"\n",
    )
    .unwrap();
    let home = path.parent().unwrap();
    let account_key = format!(
        "cli|{}",
        &hex::encode(Sha256::digest(
            home.canonicalize().unwrap().to_string_lossy().as_bytes()
        ))[..16]
    );
    let a = snapshot(ClientKind::Codex, "a", "org", "a");
    fixture
        .app
        .native_credentials
        .store(
            "Codex Auth",
            &account_key,
            &serde_json::to_string(&a.auth).unwrap(),
        )
        .unwrap();
    let saved = fixture.save(&a).await;
    fixture.switch(&saved).await;
    let destination = fixture
        .save(&snapshot(ClientKind::Codex, "destination", "org", "saved"))
        .await;
    let external = snapshot(ClientKind::Codex, "external", "org", "external");
    let text = serde_json::to_string(&external.auth).unwrap();
    fs::write(home.join("auth.json"), &text).unwrap();
    assert!(
        fixture
            .app
            .switch_provider(ProviderSwitchParams {
                client: ClientKind::Codex,
                provider_id: destination.id
            })
            .await
            .is_err()
    );
    assert!(fs::read_to_string(home.join("auth.json")).unwrap() == text);
    assert_eq!(
        fixture
            .app
            .db
            .client_state(ClientKind::Codex)
            .unwrap()
            .active_provider_id
            .as_deref(),
        Some(saved.id.as_str())
    );
    let other = fixture.other();
    let request = handoff(&fixture, &other, ClientKind::Codex);
    assert!(
        fixture
            .app
            .release_configuration(hsin_core::ConfigReleaseParams {
                request_id: request.request_id,
                target: request.targets[0].clone(),
                requester: other.instance.clone()
            })
            .await
            .is_err()
    );
    assert!(fs::read_to_string(home.join("auth.json")).unwrap() == text);
}

#[tokio::test]
async fn adding_accounts_deduplicates_identity_and_preserves_name_without_activation() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let first = fixture.save(&snapshot(client, "a", "org-a", "r1")).await;
        fixture
            .app
            .rename_official_account(OfficialAccountRenameParams {
                provider_id: first.id.clone(),
                expected_revision: first.revision,
                name: "自定义账号".into(),
            })
            .await
            .unwrap();
        let updated = fixture.save(&snapshot(client, "a", "org-a", "r2")).await;
        assert_eq!(updated.id, first.id);
        assert_eq!(updated.name, "自定义账号");
        assert_eq!(updated.revision, 2);
        let second = fixture.save(&snapshot(client, "a", "org-b", "r3")).await;
        assert_ne!(first.id, second.id);
        assert!(
            fixture
                .app
                .db
                .client_state(client)
                .unwrap()
                .active_provider_id
                .is_none()
        );
        assert!(fixture.app.db.secret(&first.id).is_err());
        assert!(
            fixture
                .app
                .db
                .get_provider(&first.id)
                .unwrap()
                .credential_preview
                .is_none()
        );
        assert_eq!(
            fixture
                .app
                .db
                .official_account(&first.id)
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
    }
}

#[tokio::test]
async fn automatic_import_skips_api_only_and_keeps_current_config() {
    let fixture = Fixture::new();
    let api = fixture.api(ClientKind::Codex);
    fixture.switch(&api).await;
    let before = fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap();
    fixture
        .app
        .capture_existing_official_accounts()
        .await
        .unwrap();
    assert_eq!(
        fixture
            .app
            .db
            .list_providers(Some(ClientKind::Codex))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        before,
        fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap()
    );
    for client in ClientKind::ALL {
        let original = snapshot(client, "native", "org", "r1");
        fixture.write(&original);
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        let record = fixture
            .app
            .db
            .find_official_account(client, "native", "org")
            .unwrap()
            .unwrap();
        let mut newer = fixture.current(client);
        refresh(&mut newer, "r2");
        fixture.write(&newer);
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        let stored = fixture
            .app
            .read_official_snapshot(&credential_key(&record.provider_id))
            .unwrap();
        assert!(if client == ClientKind::Codex {
            stored.auth["tokens"]["refresh_token"] == "r2"
        } else {
            stored.auth["claudeAiOauth"]["refreshToken"] == "r2"
        });
    }
}

#[tokio::test]
async fn official_api_round_trip_preserves_refresh_and_restores_native_login() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        fixture.write(&snapshot(client, "native", "org", "native-refresh"));
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        let a = fixture
            .save(&snapshot(client, "a", "org", "a-refresh"))
            .await;
        let b = fixture
            .save(&snapshot(client, "b", "org", "b-refresh"))
            .await;
        fixture.switch(&a).await;
        let mut newer = fixture.current(client);
        refresh(&mut newer, "a-refreshed");
        fixture.write(&newer);
        fixture.switch(&b).await;
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "b"
        );
        let api = fixture.api(client);
        fixture.switch(&api).await;
        fixture.switch(&a).await;
        let current = fixture.current(client);
        assert_eq!(current.identity.as_ref().unwrap().account_id, "a");
        assert!(if client == ClientKind::Codex {
            current.auth["tokens"]["refresh_token"] == "a-refreshed"
        } else {
            current.auth["claudeAiOauth"]["refreshToken"] == "a-refreshed"
        });
        let native = fixture.app.ensure_official_provider(client).unwrap();
        fixture.switch(&native).await;
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "native"
        );
        assert_eq!(fixture.app.db.pending_operations().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn a_new_login_is_not_overwritten_by_outgoing_old_tokens() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let a = fixture.save(&snapshot(client, "a", "org", "old")).await;
        fixture.switch(&a).await;
        fixture
            .save(&snapshot(client, "a", "org", "reauthenticated"))
            .await;
        fixture.switch(&a).await;
        let current = fixture.current(client);
        assert!(if client == ClientKind::Codex {
            current.auth["tokens"]["refresh_token"] == "reauthenticated"
        } else {
            current.auth["claudeAiOauth"]["refreshToken"] == "reauthenticated"
        });
    }
}

#[tokio::test]
async fn external_account_changes_block_switch_without_overwriting_login() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let a = fixture.save(&snapshot(client, "a", "org", "r1")).await;
        let b = fixture.save(&snapshot(client, "b", "org", "r2")).await;
        fixture.switch(&a).await;
        fixture.write(&snapshot(client, "foreign", "org", "r3"));
        assert!(
            fixture
                .app
                .switch_provider(ProviderSwitchParams {
                    client,
                    provider_id: b.id
                })
                .await
                .is_err()
        );
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "foreign"
        );
    }
}

#[tokio::test]
async fn active_accounts_cannot_be_deleted_and_inactive_tokens_are_removed() {
    let fixture = Fixture::new();
    let a = fixture
        .save(&snapshot(ClientKind::Codex, "a", "org", "r1"))
        .await;
    fixture.switch(&a).await;
    let params = ProviderRemoveParams {
        id: a.id.clone(),
        expected_revision: a.revision,
    };
    assert!(fixture.app.remove_provider(params.clone()).await.is_err());
    fixture
        .switch(
            &fixture
                .app
                .ensure_official_provider(ClientKind::Codex)
                .unwrap(),
        )
        .await;
    fixture.app.remove_provider(params).await.unwrap();
    assert!(
        fixture
            .app
            .db
            .protected_value(&credential_key(&a.id))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn prepared_switch_rejects_later_reauthentication() {
    let fixture = Fixture::new();
    let account = fixture
        .save(&snapshot(ClientKind::Codex, "a", "org", "r1"))
        .await;
    let transaction = fixture
        .app
        .begin_config_transaction(&[ClientKind::Codex], false)
        .unwrap();
    let mut target = fixture
        .app
        .config_target(&account, ConnectionMode::Direct, None)
        .unwrap();
    target.official_auth_transition = fixture
        .app
        .prepare_official_auth_transition(&account)
        .unwrap();
    let before = fixture.current(ClientKind::Codex);
    fixture
        .save(&snapshot(ClientKind::Codex, "a", "org", "r2"))
        .await;
    assert!(fixture.app.apply_official_auth_transition(&target).is_err());
    assert!(fixture.current(ClientKind::Codex).auth == before.auth);
    drop(transaction);
}

#[tokio::test]
async fn interrupted_native_switch_recovers_from_encrypted_snapshots() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let a = fixture.save(&snapshot(client, "a", "org", "r1")).await;
        let transaction = fixture
            .app
            .begin_config_transaction(&[client], false)
            .unwrap();
        let mut target = fixture
            .app
            .config_target(&a, ConnectionMode::Direct, None)
            .unwrap();
        target.official_auth_transition = fixture.app.prepare_official_auth_transition(&a).unwrap();
        let before_hash = config::file_hash(&fixture.app.config_path(client).unwrap()).unwrap();
        let json = serde_json::to_string(&target).unwrap();
        assert!(!json.contains("refresh_token"));
        assert!(!json.contains("refreshToken"));
        let operation = fixture
            .app
            .db
            .begin_operation("apply_config", client, before_hash.as_deref(), &json)
            .unwrap();
        fixture
            .app
            .start_ownership_write(&target, &operation)
            .unwrap();
        fixture.app.apply_official_auth_transition(&target).unwrap();
        drop(transaction);
        fixture.app.recover_operations().unwrap();
        assert_eq!(
            fixture
                .app
                .db
                .client_state(client)
                .unwrap()
                .active_provider_id
                .as_deref(),
            Some(a.id.as_str())
        );
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "a"
        );
        assert_eq!(fixture.app.db.pending_operations().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn display_settings_do_not_rewrite_client_configuration() {
    let fixture = Fixture::new();
    let api = fixture.api(ClientKind::Codex);
    fixture.switch(&api).await;
    let before = fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap();
    fixture
        .app
        .update_settings(SettingsPatch {
            official_account_display: Some(hsin_core::OfficialAccountDisplayUpdate {
                client: ClientKind::Codex,
                display: hsin_core::OfficialAccountDisplay::NameAndEmail,
            }),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    assert_eq!(
        fixture
            .app
            .settings()
            .unwrap()
            .client_auth
            .codex_official_account_display,
        hsin_core::OfficialAccountDisplay::NameAndEmail
    );
    assert_eq!(
        before,
        fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap()
    );
}

#[tokio::test]
async fn a_native_refresh_before_first_switch_is_saved() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        fixture.write(&snapshot(client, "native", "org", "r1"));
        fixture
            .app
            .capture_existing_official_accounts()
            .await
            .unwrap();
        let id = fixture
            .app
            .db
            .find_official_account(client, "native", "org")
            .unwrap()
            .unwrap()
            .provider_id;
        let mut newer = fixture.current(client);
        refresh(&mut newer, "latest");
        fixture.write(&newer);
        fixture
            .switch(&fixture.app.db.get_provider(&id).unwrap())
            .await;
        let current = fixture.current(client);
        assert!(if client == ClientKind::Codex {
            current.auth["tokens"]["refresh_token"] == "latest"
        } else {
            current.auth["claudeAiOauth"]["refreshToken"] == "latest"
        });
    }
}

#[tokio::test]
async fn preserved_official_auth_survives_api_account_round_trip() {
    let fixture = Fixture::new();
    fixture.write(&snapshot(ClientKind::Codex, "native", "org", "native"));
    let a = fixture
        .save(&snapshot(ClientKind::Codex, "a", "org", "saved"))
        .await;
    fixture.switch(&a).await;
    fixture
        .app
        .update_settings(SettingsPatch {
            codex_preserve_official_auth: Some(true),
            ..SettingsPatch::default()
        })
        .await
        .unwrap();
    let before = fixture.current(ClientKind::Codex);
    let api = fixture.api(ClientKind::Codex);
    fixture.switch(&api).await;
    assert!(fixture.current(ClientKind::Codex).auth == before.auth);
    fixture.switch(&a).await;
    assert!(fixture.current(ClientKind::Codex).auth == before.auth);
    assert!(
        fixture
            .app
            .settings()
            .unwrap()
            .client_auth
            .codex_preserve_official_auth
    );
}

#[tokio::test]
async fn a_half_written_claude_switch_recovers_both_resources() {
    let fixture = Fixture::new();
    let client = ClientKind::Claude;
    fixture.write(&snapshot(client, "native", "org", "native"));
    let a = fixture.save(&snapshot(client, "a", "org", "saved")).await;
    let transaction = fixture
        .app
        .begin_config_transaction(&[client], false)
        .unwrap();
    let mut target = fixture
        .app
        .config_target(&a, ConnectionMode::Direct, None)
        .unwrap();
    target.official_auth_transition = fixture.app.prepare_official_auth_transition(&a).unwrap();
    let operation = fixture
        .app
        .db
        .begin_operation(
            "apply_config",
            client,
            config::file_hash(&fixture.app.config_path(client).unwrap())
                .unwrap()
                .as_deref(),
            &serde_json::to_string(&target).unwrap(),
        )
        .unwrap();
    fixture
        .app
        .start_ownership_write(&target, &operation)
        .unwrap();
    let mut partial = fixture.current(client);
    partial.auth = snapshot(client, "a", "org", "saved").auth.clone();
    fixture.write(&partial);
    drop(transaction);
    fixture.app.recover_operations().unwrap();
    assert_eq!(
        fixture
            .current(client)
            .identity
            .as_ref()
            .unwrap()
            .account_id,
        "a"
    );
    assert_eq!(fixture.app.db.pending_operations().unwrap().len(), 0);
}

fn handoff(fixture: &Fixture, other: &App, client: ClientKind) -> hsin_core::ConfigTakeoverParams {
    let target =
        crate::ownership::Target::new(client, fixture.app.config_path(client).unwrap()).unwrap();
    let record = target.read_record().unwrap().unwrap();
    let request = hsin_core::ConfigTakeoverParams {
        request_id: uuid::Uuid::new_v4().to_string(),
        targets: vec![hsin_core::ConfigTakeoverTarget {
            client,
            target_id: target.id,
            expected_owner_id: Some(fixture.app.instance.instance_id.clone()),
            expected_generation: record.generation,
        }],
    };
    assert_ne!(fixture.app.instance.instance_id, other.instance.instance_id);
    request
}

#[tokio::test]
async fn cooperative_handoff_restores_native_and_keeps_vaults_isolated() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let other = fixture.other();
        fixture.write(&snapshot(client, "native", "org", "native"));
        let a = fixture.save(&snapshot(client, "a", "org", "saved")).await;
        fixture.switch(&a).await;
        let request = handoff(&fixture, &other, client);
        fixture
            .app
            .release_configuration(hsin_core::ConfigReleaseParams {
                request_id: request.request_id.clone(),
                target: request.targets[0].clone(),
                requester: other.instance.clone(),
            })
            .await
            .unwrap();
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "native"
        );
        other.takeover_configuration(request).await.unwrap();
        other.capture_existing_official_accounts().await.unwrap();
        assert!(other.db.get_provider(&a.id).is_err());
        assert!(
            other
                .db
                .find_official_account(client, "native", "org")
                .unwrap()
                .is_some()
        );
        assert!(fixture.app.db.get_provider(&a.id).is_ok());
    }
}

#[tokio::test]
async fn external_login_between_release_and_claim_blocks_handoff() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let other = fixture.other();
        fixture.write(&snapshot(client, "native", "org", "native"));
        let a = fixture.save(&snapshot(client, "a", "org", "saved")).await;
        fixture.switch(&a).await;
        let request = handoff(&fixture, &other, client);
        fixture
            .app
            .release_configuration(hsin_core::ConfigReleaseParams {
                request_id: request.request_id.clone(),
                target: request.targets[0].clone(),
                requester: other.instance.clone(),
            })
            .await
            .unwrap();
        fixture.write(&snapshot(client, "external", "org", "external"));
        assert!(other.takeover_configuration(request).await.is_err());
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "external"
        );
    }
}

#[tokio::test]
async fn interrupted_handoff_reuses_the_original_restore_snapshot() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let other = fixture.other();
        fixture.write(&snapshot(client, "native", "org", "native"));
        let a = fixture.save(&snapshot(client, "a", "org", "saved")).await;
        fixture.switch(&a).await;
        let request = handoff(&fixture, &other, client);
        let params = hsin_core::ConfigReleaseParams {
            request_id: request.request_id.clone(),
            target: request.targets[0].clone(),
            requester: other.instance.clone(),
        };
        let transaction = fixture
            .app
            .begin_config_transaction(&[client], false)
            .unwrap();
        let official = fixture.app.ensure_official_provider(client).unwrap();
        let mut restore = fixture
            .app
            .config_target(&official, ConnectionMode::Direct, None)
            .unwrap();
        restore.official_auth_transition = fixture
            .app
            .prepare_official_auth_transition(&official)
            .unwrap();
        let restore_key = format!(
            "official_release_target:{}:{}",
            params.request_id, params.target.target_id
        );
        fixture
            .app
            .db
            .set_setting(&restore_key, &serde_json::to_string(&restore).unwrap())
            .unwrap();
        fixture
            .app
            .db
            .begin_operation(
                "release_config",
                client,
                None,
                &serde_json::to_string(&params).unwrap(),
            )
            .unwrap();
        let target =
            crate::ownership::Target::new(client, fixture.app.config_path(client).unwrap())
                .unwrap();
        {
            let mut guards = fixture.app.ownership_guards.lock();
            let guard = guards.get_mut(&target.path).unwrap();
            let mut record = guard.record().cloned().unwrap();
            record.pending = Some(crate::ownership::Pending {
                request_id: params.request_id.clone(),
                kind: crate::ownership::PendingKind::Release,
                requester: Some(params.requester.clone()),
            });
            guard.set_record(record).unwrap();
        }
        config::apply_with_credential(
            &target.config_path,
            config::file_hash(&target.config_path).unwrap().as_deref(),
            &restore,
            None,
        )
        .unwrap();
        fixture
            .app
            .apply_official_auth_transition(&restore)
            .unwrap();
        drop(transaction);
        // Crash before applied-state commit: the old state still points at A.
        fixture.app.recover_operations().unwrap();
        assert_eq!(
            fixture
                .current(client)
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "native"
        );
        assert!(fixture.app.db.setting(&restore_key).unwrap().is_none());
        assert_eq!(fixture.app.db.pending_operations().unwrap().len(), 0);
        other.takeover_configuration(request).await.unwrap();
    }
}

#[tokio::test]
async fn removed_native_accounts_are_not_reimported_until_explicit_login() {
    let fixture = Fixture::new();
    let client = ClientKind::Codex;
    fixture.write(&snapshot(client, "native", "org", "native"));
    fixture
        .app
        .capture_existing_official_accounts()
        .await
        .unwrap();
    let record = fixture
        .app
        .db
        .find_official_account(client, "native", "org")
        .unwrap()
        .unwrap();
    let provider = fixture.app.db.get_provider(&record.provider_id).unwrap();
    fixture
        .app
        .remove_provider(ProviderRemoveParams {
            id: provider.id,
            expected_revision: provider.revision,
        })
        .await
        .unwrap();
    fixture
        .app
        .capture_existing_official_accounts()
        .await
        .unwrap();
    assert!(
        fixture
            .app
            .db
            .find_official_account(client, "native", "org")
            .unwrap()
            .is_none()
    );
    fixture
        .save(&snapshot(client, "native", "org", "explicit"))
        .await;
    assert!(
        fixture
            .app
            .db
            .find_official_account(client, "native", "org")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn equal_credential_revisions_cannot_overwrite_encrypted_records() {
    let fixture = Fixture::new();
    let account = fixture
        .save(&snapshot(ClientKind::Codex, "a", "org", "r1"))
        .await;
    let record = fixture
        .app
        .db
        .official_account(&account.id)
        .unwrap()
        .unwrap();
    let encrypted = fixture
        .app
        .db
        .protected_value(&credential_key(&account.id))
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .app
            .db
            .save_official_account(
                &account,
                &record,
                account.official_account.as_ref().unwrap(),
                &encrypted
            )
            .is_err()
    );
    assert!(
        fixture
            .app
            .db
            .protected_value(&credential_key(&account.id))
            .unwrap()
            .unwrap()
            .ciphertext
            == encrypted.ciphertext
    );
}

#[tokio::test]
async fn a_saved_codex_account_activates_openai_from_an_unmanaged_custom_selector() {
    let fixture = Fixture::new();
    let client = ClientKind::Codex;
    let path = fixture.app.config_path(client).unwrap();
    fs::write(&path,"# user configuration\r\nmodel_provider = \"acme\" # selection\r\n\r\n[model_providers.acme]\r\nbase_url = \"https://acme.example.test/v1\"\r\n\r\n[mcp_servers.keep]\r\ncommand = \"unchanged\"\r\n").unwrap();
    let a = fixture.save(&snapshot(client, "a", "org", "saved")).await;
    fixture.switch(&a).await;
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# user configuration\r\nmodel_provider = \"openai\" # selection\r\n\r\n[model_providers.acme]\r\nbase_url = \"https://acme.example.test/v1\"\r\n\r\n[mcp_servers.keep]\r\ncommand = \"unchanged\"\r\n"
    );
}
