use super::*;

struct Fixture {
    home: PathBuf,
    credentials: Arc<MemoryNativeCredentialStore>,
}

impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("hsin-native-auth-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        Self {
            home,
            credentials: Arc::new(MemoryNativeCredentialStore::default()),
        }
    }

    fn codex(&self, configuration: &str) -> NativeAuthStore {
        let path = self.home.join("config.toml");
        fs::write(&path, configuration).unwrap();
        NativeAuthStore::with_credentials(ClientKind::Codex, &path, true, self.credentials.clone())
            .unwrap()
    }

    fn codex_backend(&self, configuration: &str, keyring: KeyringBackend) -> NativeAuthStore {
        let mut store = self.codex(configuration);
        if let Backend::Codex {
            keyring: selected, ..
        } = &mut store.backend
        {
            *selected = keyring;
        }
        store
    }

    fn claude(&self, keychain: bool) -> NativeAuthStore {
        let path = self.home.join("settings.json");
        let mut store = NativeAuthStore::with_credentials(
            ClientKind::Claude,
            &path,
            true,
            self.credentials.clone(),
        )
        .unwrap();
        store.lock_path = self.home.join(".native-test.lock");
        store.backend = if keychain {
            Backend::ClaudeKeychain {
                service: claude_service(&self.home, true),
                account: "test-user".into(),
            }
        } else {
            Backend::ClaudeFile
        };
        store
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn codex_auth(user: &str, workspace: &str, refresh: &str) -> Value {
    let payload = serde_json::json!({ "sub": user, "email": "account@example.test", "name": "Native name", "https://api.openai.com/auth": { "chatgpt_user_id": user, "chatgpt_account_id": workspace } });
    let jwt = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    );
    serde_json::json!({ "auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": { "id_token": jwt, "access_token": "test-access", "refresh_token": refresh, "account_id": workspace }, "last_refresh": "2026-10-08T00:00:00Z" })
}

fn snapshot(client: ClientKind, auth: Value, metadata: Value) -> NativeAuthSnapshot {
    NativeAuthSnapshot {
        client,
        identity: native_identity(client, &auth, &metadata),
        auth,
        metadata,
    }
}

#[test]
fn codex_file_switch_preserves_unknown_bytes_and_permissions() {
    let fixture = Fixture::new();
    let store = fixture.codex("");
    let before = "{\r\n  // unrelated login extension\r\n  \"agent_extension\": { \"note\": \"心\" },\r\n  \"OPENAI_API_KEY\" : \"old-test-key\",\r\n  \"auth_mode\": \"apikey\"\r\n}\r\n";
    fs::write(&store.auth_path, before).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&store.auth_path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let hash = store.fingerprint().unwrap();
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("user-a", "workspace-a", "refresh-a"),
        serde_json::json!({}),
    );
    let _guard = store.lock_for_switch().unwrap();
    store.apply(&next, &hash).unwrap();
    let text = fs::read_to_string(&store.auth_path).unwrap();
    assert!(text.contains(
        "  // unrelated login extension\r\n  \"agent_extension\": { \"note\": \"心\" },\r\n"
    ));
    assert!(!text.contains("old-test-key"));
    let captured = store.capture().unwrap();
    assert_eq!(captured.identity.as_ref().unwrap().account_id, "user-a");
    assert_eq!(
        captured.identity.as_ref().unwrap().organization_id,
        "workspace-a"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&store.auth_path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn codex_external_refresh_fails_cas_without_changes() {
    let fixture = Fixture::new();
    let store = fixture.codex("");
    fs::write(
        &store.auth_path,
        codex_auth("user-a", "workspace-a", "refresh-a").to_string(),
    )
    .unwrap();
    let hash = store.fingerprint().unwrap();
    let refreshed = codex_auth("user-a", "workspace-a", "refresh-b").to_string();
    fs::write(&store.auth_path, &refreshed).unwrap();
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("user-b", "workspace-b", "refresh-c"),
        serde_json::json!({}),
    );
    assert!(matches!(
        store.apply(&next, &hash),
        Err(DaemonError::Conflict(_))
    ));
    assert_eq!(fs::read_to_string(&store.auth_path).unwrap(), refreshed);
}

#[test]
fn codex_unowned_edit_survives_switch() {
    let fixture = Fixture::new();
    let store = fixture.codex("");
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"test-old\",\"other\":1}",
    )
    .unwrap();
    let hash = store.fingerprint().unwrap();
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"test-old\",\"other\":2}",
    )
    .unwrap();
    let next = snapshot(
        ClientKind::Codex,
        serde_json::json!({"OPENAI_API_KEY":"test-new"}),
        serde_json::json!({}),
    );
    store.apply(&next, &hash).unwrap();
    assert_eq!(
        fs::read_to_string(&store.auth_path).unwrap(),
        "{\"OPENAI_API_KEY\":\"test-new\",\"other\":2}"
    );
}

#[test]
fn empty_baseline_restores_only_owned_fields() {
    let fixture = Fixture::new();
    let store = fixture.codex("");
    let baseline = store.capture().unwrap();
    assert!(baseline.identity.is_none());
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("a", "org-a", "refresh-a"),
        serde_json::json!({}),
    );
    store.apply(&next, &store.fingerprint().unwrap()).unwrap();
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&store.auth_path).unwrap()).unwrap();
    value["extension"] = serde_json::json!({"keep":"心"});
    fs::write(&store.auth_path, value.to_string()).unwrap();
    store
        .apply(&baseline, &store.fingerprint().unwrap())
        .unwrap();
    assert_eq!(store.capture().unwrap().auth, serde_json::json!({}));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&store.auth_path).unwrap()).unwrap(),
        serde_json::json!({"extension":{"keep":"心"}})
    );
}

#[test]
fn codex_direct_keyring_keeps_extensions_and_cleans_owned_fallback() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"keyring\"\n",
        KeyringBackend::Direct,
    );
    let account = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(
            CODEX_AUTH_SERVICE,
            &account,
            "{\"OPENAI_API_KEY\":\"test-a\",\"other\":{\"unchanged\":true}}",
        )
        .unwrap();
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"test-stale\",\"file_extension\":true}",
    )
    .unwrap();
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("user-b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    store.apply(&next, &store.fingerprint().unwrap()).unwrap();
    let keyring = fixture
        .credentials
        .load(CODEX_AUTH_SERVICE, &account)
        .unwrap()
        .unwrap();
    assert!(keyring.contains("\"other\":{\"unchanged\":true}"));
    assert_eq!(
        store
            .capture()
            .unwrap()
            .identity
            .as_ref()
            .unwrap()
            .account_id,
        "user-b"
    );
    assert_eq!(
        projection(&fs::read_to_string(&store.auth_path).unwrap(), CODEX_KEYS).unwrap(),
        serde_json::json!({})
    );
    assert!(
        fs::read_to_string(&store.auth_path)
            .unwrap()
            .contains("\"file_extension\":true")
    );
}

#[test]
fn codex_recovery_finishes_secure_write_before_fallback_cleanup() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"keyring\"\n",
        KeyringBackend::Direct,
    );
    let account = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(
            CODEX_AUTH_SERVICE,
            &account,
            &codex_auth("a", "org-a", "refresh-a").to_string(),
        )
        .unwrap();
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"stale-test\",\"extension\":\"心\"}",
    )
    .unwrap();
    let before = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after = snapshot(
        ClientKind::Codex,
        codex_auth("b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    // Simulate interruption immediately after the secure store was committed.
    fixture
        .credentials
        .store(CODEX_AUTH_SERVICE, &account, &after.auth.to_string())
        .unwrap();
    store.recover_apply(&before, &after, &prepared).unwrap();
    assert_eq!(store.capture().unwrap().auth, after.auth);
    assert_eq!(
        projection(&fs::read_to_string(&store.auth_path).unwrap(), CODEX_KEYS).unwrap(),
        serde_json::json!({})
    );
    assert!(
        fs::read_to_string(&store.auth_path)
            .unwrap()
            .contains("\"extension\":\"心\"")
    );
    // A completed recovery is idempotent even though cleanup changed its digest.
    store.recover_apply(&before, &after, &prepared).unwrap();
}

#[test]
fn codex_recovery_refuses_new_fallback_login_before_or_after_secure_commit() {
    for secure_committed in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.codex_backend(
            "cli_auth_credentials_store = \"keyring\"\n",
            KeyringBackend::Direct,
        );
        let account = codex_account(&fixture.home, "cli");
        fixture
            .credentials
            .store(
                CODEX_AUTH_SERVICE,
                &account,
                &codex_auth("a", "org-a", "refresh-a").to_string(),
            )
            .unwrap();
        fs::write(&store.auth_path, "{\"OPENAI_API_KEY\":\"stale-test\"}").unwrap();
        let before = store.capture().unwrap();
        let prepared = store.fingerprint().unwrap();
        let after = snapshot(
            ClientKind::Codex,
            codex_auth("b", "org-b", "refresh-b"),
            serde_json::json!({}),
        );
        if secure_committed {
            fixture
                .credentials
                .store(CODEX_AUTH_SERVICE, &account, &after.auth.to_string())
                .unwrap();
        }
        let external = codex_auth("external", "external-org", "external-refresh").to_string();
        fs::write(&store.auth_path, &external).unwrap();
        let secure = fixture
            .credentials
            .load(CODEX_AUTH_SERVICE, &account)
            .unwrap();
        assert!(matches!(
            store.recover_apply(&before, &after, &prepared),
            Err(DaemonError::Conflict(_))
        ));
        assert!(matches!(
            store.recover_rollback(&before, &after, &prepared, "keyring"),
            Err(DaemonError::Conflict(_))
        ));
        assert_eq!(fs::read_to_string(&store.auth_path).unwrap(), external);
        assert_eq!(
            fixture
                .credentials
                .load(CODEX_AUTH_SERVICE, &account)
                .unwrap(),
            secure
        );
    }
}

#[test]
fn codex_auto_rollback_restores_original_secure_store_without_plaintext_tokens() {
    for fallback_cleared in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.codex_backend(
            "cli_auth_credentials_store = \"auto\"\n",
            KeyringBackend::Direct,
        );
        let account = codex_account(&fixture.home, "cli");
        fixture
            .credentials
            .store(
                CODEX_AUTH_SERVICE,
                &account,
                &codex_auth("a", "org-a", "refresh-a").to_string(),
            )
            .unwrap();
        fs::write(
            &store.auth_path,
            "{\"OPENAI_API_KEY\":\"stale-test\",\"extension\":true}",
        )
        .unwrap();
        let (before, prepared, source) = store.capture_for_switch().unwrap();
        assert_eq!(source, "keyring");
        let after = snapshot(
            ClientKind::Codex,
            serde_json::json!({}),
            serde_json::json!({}),
        );
        fixture
            .credentials
            .delete(CODEX_AUTH_SERVICE, &account)
            .unwrap();
        if fallback_cleared {
            fs::write(&store.auth_path, "{\"extension\":true}").unwrap();
        }
        store
            .recover_rollback(&before, &after, &prepared, &source)
            .unwrap();
        assert_eq!(store.capture().unwrap().auth, before.auth);
        assert!(
            fixture
                .credentials
                .load(CODEX_AUTH_SERVICE, &account)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&fs::read_to_string(&store.auth_path).unwrap()).unwrap(),
            serde_json::json!({"extension":true})
        );
        store
            .recover_rollback(&before, &after, &prepared, &source)
            .unwrap();
    }
}

#[test]
fn codex_auto_rollback_rejects_an_external_secure_source_and_ambiguous_old_journal() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    fs::write(
        &store.auth_path,
        codex_auth("a", "org-a", "refresh-a").to_string(),
    )
    .unwrap();
    let (before, prepared, source) = store.capture_for_switch().unwrap();
    let after = snapshot(
        ClientKind::Codex,
        codex_auth("b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    let account = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(CODEX_AUTH_SERVICE, &account, &after.auth.to_string())
        .unwrap();
    assert!(matches!(
        store.recover_rollback(&before, &after, &prepared, &source),
        Err(DaemonError::Conflict(_))
    ));
    assert_eq!(store.capture().unwrap().auth, after.auth);

    fs::write(&store.auth_path, "{\"OPENAI_API_KEY\":\"stale-test\"}").unwrap();
    let (secure_before, secure_prepared, _) = store.capture_for_switch().unwrap();
    let empty = snapshot(
        ClientKind::Codex,
        serde_json::json!({}),
        serde_json::json!({}),
    );
    fixture
        .credentials
        .delete(CODEX_AUTH_SERVICE, &account)
        .unwrap();
    fs::write(&store.auth_path, "{}").unwrap();
    assert!(matches!(
        store.recover_rollback(&secure_before, &empty, &secure_prepared, ""),
        Err(DaemonError::Conflict(_))
    ));
    assert!(
        fixture
            .credentials
            .load(CODEX_AUTH_SERVICE, &account)
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read_to_string(&store.auth_path).unwrap(), "{}");
}

#[test]
fn claude_rollback_restores_known_partial_auth_metadata() {
    let fixture = Fixture::new();
    let store = fixture.claude(false);
    fs::write(
        &store.auth_path,
        "{\"claudeAiOauth\":{\"accessToken\":\"a\"},\"extension\":1}",
    )
    .unwrap();
    let metadata = store.metadata_path.as_ref().unwrap();
    fs::write(
        metadata,
        "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"},\"projects\":{}}",
    )
    .unwrap();
    let (before, prepared, source) = store.capture_for_switch().unwrap();
    let after = snapshot(
        ClientKind::Claude,
        serde_json::json!({"claudeAiOauth":{"accessToken":"b"}}),
        serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b"}}),
    );
    fs::write(
        &store.auth_path,
        "{\"claudeAiOauth\":{\"accessToken\":\"b\"},\"extension\":2}",
    )
    .unwrap();
    store
        .recover_rollback(&before, &after, &prepared, &source)
        .unwrap();
    let current = store.capture().unwrap();
    assert_eq!(current.auth, before.auth);
    assert_eq!(current.metadata, before.metadata);
    assert!(
        fs::read_to_string(&store.auth_path)
            .unwrap()
            .contains("\"extension\":2")
    );
}

struct RefreshingCredentials {
    reads: std::sync::atomic::AtomicUsize,
}

impl NativeCredentialStore for RefreshingCredentials {
    fn load(&self, _: &str, _: &str) -> Result<Option<String>> {
        let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(
            codex_auth(
                "a",
                "org-a",
                if read == 0 { "refresh-a" } else { "refresh-b" },
            )
            .to_string(),
        ))
    }

    fn store(&self, _: &str, _: &str, _: &str) -> Result<()> {
        unreachable!()
    }
    fn delete(&self, _: &str, _: &str) -> Result<()> {
        unreachable!()
    }
}

#[test]
fn capture_for_switch_snapshot_digest_and_source_share_one_read() {
    let fixture = Fixture::new();
    let mut store = fixture.codex_backend(
        "cli_auth_credentials_store = \"keyring\"\n",
        KeyringBackend::Direct,
    );
    let credentials = Arc::new(RefreshingCredentials {
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    store.credentials = credentials.clone();
    let (before, fingerprint, source) = store.capture_for_switch().unwrap();
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(source, "keyring");
    assert_eq!(before.auth["tokens"]["refresh_token"], "refresh-a");
    let raw = RawState {
        source: Source::Keyring,
        auth: Zeroizing::new(before.auth.to_string()),
        metadata: Zeroizing::new(String::new()),
        file_auth: Zeroizing::new(String::new()),
        secure_available: true,
    };
    assert_eq!(store.state_fingerprint(&raw).unwrap(), fingerprint);
    assert_ne!(store.fingerprint().unwrap(), fingerprint);
}

#[test]
fn ownership_fingerprint_accepts_active_token_and_profile_refreshes() {
    for client in ClientKind::ALL {
        let fixture = Fixture::new();
        let store = match client {
            ClientKind::Codex => fixture.codex(""),
            ClientKind::Claude => fixture.claude(false),
        };
        let initial = match client {
            ClientKind::Codex => codex_auth("a", "org-a", "refresh-a"),
            ClientKind::Claude => {
                serde_json::json!({"claudeAiOauth":{"accessToken":"a","refreshToken":"refresh-a","scopes":["user:inference"]}})
            }
        };
        fs::write(&store.auth_path, initial.to_string()).unwrap();
        if let Some(metadata) = &store.metadata_path {
            fs::write(metadata, "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\",\"emailAddress\":\"a@example.test\",\"displayName\":\"Old\"}}").unwrap();
        }
        let fingerprint = store.ownership_fingerprint().unwrap();
        let mut refreshed = match client {
            ClientKind::Codex => codex_auth("a", "org-a", "refresh-b"),
            ClientKind::Claude => {
                serde_json::json!({"claudeAiOauth":{"accessToken":"b","refreshToken":"refresh-b","expiresAt":1234,"scopes":["user:inference"]}})
            }
        };
        if client == ClientKind::Codex {
            let claims = serde_json::json!({"sub":"a","email":"new@example.test","name":"New","https://api.openai.com/auth":{"chatgpt_user_id":"a","chatgpt_account_id":"org-a"}});
            refreshed["tokens"]["id_token"] = serde_json::json!(format!(
                "e30.{}.sig",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            ));
        }
        refreshed["unowned"] = serde_json::json!("retained");
        fs::write(&store.auth_path, refreshed.to_string()).unwrap();
        if let Some(metadata) = &store.metadata_path {
            fs::write(metadata, "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\",\"emailAddress\":\"new@example.test\",\"displayName\":\"New\"},\"unowned\":true}").unwrap();
        }
        assert_eq!(store.ownership_fingerprint().unwrap(), fingerprint);
        if client == ClientKind::Codex {
            refreshed["tokens"]["account_id"] = serde_json::json!("other-org");
            fs::write(&store.auth_path, refreshed.to_string()).unwrap();
        } else {
            fs::write(
                store.metadata_path.as_ref().unwrap(),
                "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"other-org\"}}",
            )
            .unwrap();
        }
        assert_ne!(store.ownership_fingerprint().unwrap(), fingerprint);
    }
}

#[test]
fn ownership_fingerprint_detects_source_and_inactive_fallback_changes() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    let active = codex_auth("a", "org-a", "refresh-a");
    fs::write(&store.auth_path, active.to_string()).unwrap();
    let file_fingerprint = store.ownership_fingerprint().unwrap();
    let account = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(CODEX_AUTH_SERVICE, &account, &active.to_string())
        .unwrap();
    assert_ne!(store.ownership_fingerprint().unwrap(), file_fingerprint);
    fs::write(&store.auth_path, "{\"unowned\":true}").unwrap();
    let secure_fingerprint = store.ownership_fingerprint().unwrap();
    fs::write(
        &store.auth_path,
        codex_auth("b", "org-b", "fallback-a").to_string(),
    )
    .unwrap();
    let fallback_fingerprint = store.ownership_fingerprint().unwrap();
    assert_ne!(fallback_fingerprint, secure_fingerprint);
    fs::write(
        &store.auth_path,
        codex_auth("b", "org-b", "fallback-b").to_string(),
    )
    .unwrap();
    let refreshed_fallback = store.ownership_fingerprint().unwrap();
    assert_ne!(refreshed_fallback, fallback_fingerprint);
    fixture
        .credentials
        .store(
            CODEX_AUTH_SERVICE,
            &account,
            &codex_auth("a", "org-a", "refresh-b").to_string(),
        )
        .unwrap();
    assert_eq!(store.ownership_fingerprint().unwrap(), refreshed_fallback);
}

#[test]
fn ownership_fingerprint_protects_unrecognized_auth_metadata_and_api_wrappers() {
    let fixture = Fixture::new();
    let store = fixture.claude(false);
    let empty = store.ownership_fingerprint().unwrap();
    let console = serde_json::json!({"claudeAiOauth":{"accessToken":"console-a","scopes":["org:create_api_key"]}});
    fs::write(&store.auth_path, console.to_string()).unwrap();
    assert!(store.capture().unwrap().identity.is_none());
    let console_fingerprint = store.ownership_fingerprint().unwrap();
    assert_ne!(console_fingerprint, empty);
    let mut changed = console;
    changed["claudeAiOauth"]["accessToken"] = serde_json::json!("console-b");
    fs::write(&store.auth_path, changed.to_string()).unwrap();
    let changed_auth = store.ownership_fingerprint().unwrap();
    assert_ne!(changed_auth, console_fingerprint);
    fs::write(
        store.metadata_path.as_ref().unwrap(),
        "{\"oauthAccount\":{\"displayName\":\"Console\"}}",
    )
    .unwrap();
    assert_ne!(store.ownership_fingerprint().unwrap(), changed_auth);

    let fixture = Fixture::new();
    let store = fixture.codex("");
    let mut auth = codex_auth("a", "org-a", "refresh-a");
    fs::write(&store.auth_path, auth.to_string()).unwrap();
    let original = store.ownership_fingerprint().unwrap();
    auth["auth_mode"] = serde_json::json!("apikey");
    auth["OPENAI_API_KEY"] = serde_json::json!("test-key");
    fs::write(&store.auth_path, auth.to_string()).unwrap();
    assert_ne!(store.ownership_fingerprint().unwrap(), original);
}

#[test]
fn ownership_fingerprint_detects_secure_availability_without_writing_fallback() {
    let fixture = Fixture::new();
    let mut store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    let file = codex_auth("a", "org-a", "refresh-a").to_string();
    fs::write(&store.auth_path, &file).unwrap();
    let available = store.ownership_fingerprint().unwrap();
    store.credentials = Arc::new(UnavailableCredentials);
    assert_ne!(store.ownership_fingerprint().unwrap(), available);
    assert_eq!(fs::read_to_string(&store.auth_path).unwrap(), file);
}

#[test]
fn ownership_fingerprint_uses_one_immediate_native_read() {
    let fixture = Fixture::new();
    let mut store = fixture.codex_backend(
        "cli_auth_credentials_store = \"keyring\"\n",
        KeyringBackend::Direct,
    );
    let credentials = Arc::new(RefreshingCredentials {
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    store.credentials = credentials.clone();
    let fingerprint = store.ownership_fingerprint().unwrap();
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(store.ownership_fingerprint().unwrap(), fingerprint);
    assert_eq!(
        credentials.reads.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[test]
fn native_identity_rejects_api_only_empty_oauth_and_console_accounts() {
    let codex_fixture = Fixture::new();
    let codex = codex_fixture.codex("");
    for auth in [
        serde_json::json!({"OPENAI_API_KEY":"test-key","auth_mode":"apikey"}),
        serde_json::json!({}),
        serde_json::json!({"tokens":{"id_token":"not-a-jwt","access_token":"a","refresh_token":"r"}}),
    ] {
        fs::write(&codex.auth_path, auth.to_string()).unwrap();
        assert!(codex.capture().unwrap().identity.is_none());
    }
    for key in ["access_token", "refresh_token"] {
        let mut auth = codex_auth("a", "org-a", "refresh-a");
        auth["tokens"][key] = serde_json::json!(" ");
        fs::write(&codex.auth_path, auth.to_string()).unwrap();
        assert!(codex.capture().unwrap().identity.is_none());
    }
    let claude_fixture = Fixture::new();
    let claude = claude_fixture.claude(false);
    fs::write(
        claude.metadata_path.as_ref().unwrap(),
        "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"}}",
    )
    .unwrap();
    for auth in [
        serde_json::json!({"claudeAiOauth":{}}),
        serde_json::json!({"claudeAiOauth":{"accessToken":" ","scopes":["user:inference"]}}),
        serde_json::json!({"claudeAiOauth":{"accessToken":"console-test","scopes":["org:create_api_key","user:profile"]}}),
    ] {
        fs::write(&claude.auth_path, auth.to_string()).unwrap();
        assert!(claude.capture().unwrap().identity.is_none());
    }
    fs::write(
        &claude.auth_path,
        "{\"claudeAiOauth\":{\"accessToken\":\"valid-test\",\"scopes\":[\"user:inference\"]}}",
    )
    .unwrap();
    assert_eq!(
        claude
            .capture()
            .unwrap()
            .identity
            .as_ref()
            .unwrap()
            .account_id,
        "a"
    );
}

#[test]
fn codex_auto_recovery_finishes_deleted_secure_entry_and_cleans_known_fallback() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    let account = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(
            CODEX_AUTH_SERVICE,
            &account,
            &codex_auth("a", "org-a", "refresh-a").to_string(),
        )
        .unwrap();
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"stale-test\",\"extension\":true}",
    )
    .unwrap();
    let before = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after = snapshot(
        ClientKind::Codex,
        serde_json::json!({}),
        serde_json::json!({}),
    );
    fixture
        .credentials
        .delete(CODEX_AUTH_SERVICE, &account)
        .unwrap();
    assert_ne!(store.capture().unwrap().auth, before.auth);
    store.recover_apply(&before, &after, &prepared).unwrap();
    assert_eq!(store.capture().unwrap().auth, after.auth);
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&store.auth_path).unwrap()).unwrap(),
        serde_json::json!({"extension":true})
    );
    store.recover_apply(&before, &after, &prepared).unwrap();
}

#[test]
fn codex_auto_recovery_proves_original_file_backend() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    fs::write(&store.auth_path, "{\"OPENAI_API_KEY\":\"before-test\"}").unwrap();
    let before = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after = snapshot(
        ClientKind::Codex,
        codex_auth("b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    store.recover_apply(&before, &after, &prepared).unwrap();
    assert_eq!(store.capture().unwrap().auth, after.auth);
    assert!(
        fixture
            .credentials
            .load(CODEX_AUTH_SERVICE, &codex_account(&fixture.home, "cli"))
            .unwrap()
            .is_none()
    );
}

struct UnavailableCredentials;

impl NativeCredentialStore for UnavailableCredentials {
    fn load(&self, _: &str, _: &str) -> Result<Option<String>> {
        Err(native_keyring_error())
    }

    fn store(&self, _: &str, _: &str, _: &str) -> Result<()> {
        panic!("unavailable secure store must not be written")
    }

    fn delete(&self, _: &str, _: &str) -> Result<()> {
        panic!("unavailable secure store must not be deleted")
    }
}

#[test]
fn codex_auto_can_read_unavailable_keyring_fallback_but_cannot_write_it() {
    let fixture = Fixture::new();
    let mut store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    store.credentials = Arc::new(UnavailableCredentials);
    let original = "{\"OPENAI_API_KEY\":\"existing-test\"}";
    fs::write(&store.auth_path, original).unwrap();
    let before = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after = snapshot(
        ClientKind::Codex,
        codex_auth("b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    assert!(matches!(
        store.apply(&after, &prepared),
        Err(DaemonError::Locked)
    ));
    assert!(matches!(
        store.recover_apply(&before, &after, &prepared),
        Err(DaemonError::Locked)
    ));
    assert_eq!(fs::read_to_string(&store.auth_path).unwrap(), original);
}

#[test]
fn claude_recovery_accepts_only_journaled_auth_metadata_half_states() {
    for auth_committed in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.claude(false);
        fs::write(
            &store.auth_path,
            "{\"claudeAiOauth\":{\"accessToken\":\"a\"},\"extension\":1}",
        )
        .unwrap();
        let metadata_path = store.metadata_path.as_ref().unwrap();
        fs::write(metadata_path, "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"},\"projects\":{}}").unwrap();
        let before = store.capture().unwrap();
        let prepared = store.fingerprint().unwrap();
        let after = snapshot(
            ClientKind::Claude,
            serde_json::json!({"claudeAiOauth":{"accessToken":"b"}}),
            serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b"}}),
        );
        if auth_committed {
            fs::write(&store.auth_path, after.auth.to_string()).unwrap();
        } else {
            fs::write(metadata_path, after.metadata.to_string()).unwrap();
        }
        store.recover_apply(&before, &after, &prepared).unwrap();
        let current = store.capture().unwrap();
        assert_eq!(current.auth, after.auth);
        assert_eq!(current.metadata, after.metadata);
    }
}

#[test]
fn codex_auto_keeps_resolved_file_until_native_keyring_changes() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Direct,
    );
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("user-a", "org-a", "refresh-a"),
        serde_json::json!({}),
    );
    let hash = store.fingerprint().unwrap();
    store.apply(&next, &hash).unwrap();
    assert!(
        fixture
            .credentials
            .load(CODEX_AUTH_SERVICE, &codex_account(&fixture.home, "cli"))
            .unwrap()
            .is_none()
    );
    let before = store.fingerprint().unwrap();
    fixture
        .credentials
        .store(
            CODEX_AUTH_SERVICE,
            &codex_account(&fixture.home, "cli"),
            &codex_auth("user-b", "org-b", "refresh-b").to_string(),
        )
        .unwrap();
    assert!(matches!(
        store.apply(&next, &before),
        Err(DaemonError::Conflict(_))
    ));
}

#[test]
fn codex_secrets_age_roundtrip_preserves_other_entries_and_shared_key() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"auto\"\n",
        KeyringBackend::Secrets,
    );
    let account = codex_account(&fixture.home, "secrets");
    fixture
        .credentials
        .store(
            CODEX_SECRETS_SERVICE,
            &account,
            "high-entropy-test-passphrase",
        )
        .unwrap();
    let mut file = serde_json::json!({"version":1,"secrets":{"global/CODEX_AUTH":codex_auth("a","org-a","refresh-a").to_string(),"global/OTHER":"unchanged-test-secret"},"extension":{"keep":true}});
    let recipient = age::scrypt::Recipient::new(age::secrecy::SecretString::from(
        "high-entropy-test-passphrase".to_string(),
    ));
    fs::create_dir_all(fixture.home.join("secrets")).unwrap();
    fs::write(
        store.secrets_path(),
        age::encrypt(&recipient, &serde_json::to_vec(&file).unwrap()).unwrap(),
    )
    .unwrap();
    fs::write(
        fixture.home.join("secrets/mcp_oauth.age"),
        b"unrelated ciphertext",
    )
    .unwrap();
    let next = snapshot(
        ClientKind::Codex,
        codex_auth("b", "org-b", "refresh-b"),
        serde_json::json!({}),
    );
    store.apply(&next, &store.fingerprint().unwrap()).unwrap();
    assert_eq!(
        store
            .capture()
            .unwrap()
            .identity
            .as_ref()
            .unwrap()
            .account_id,
        "b"
    );
    let plaintext = decrypt_secrets(
        &fs::read(store.secrets_path()).unwrap(),
        "high-entropy-test-passphrase".into(),
    )
    .unwrap();
    let restored: Value = serde_json::from_str(&plaintext).unwrap();
    assert_eq!(restored["secrets"]["global/OTHER"], "unchanged-test-secret");
    assert_eq!(restored["extension"], file["extension"]);
    assert_eq!(
        fs::read(fixture.home.join("secrets/mcp_oauth.age")).unwrap(),
        b"unrelated ciphertext"
    );
    assert_eq!(
        fixture
            .credentials
            .load(CODEX_SECRETS_SERVICE, &account)
            .unwrap()
            .as_deref(),
        Some("high-entropy-test-passphrase")
    );
    exercise_secrets_recovery_and_rollback(&store);
    wipe_json(&mut file);
}

fn exercise_secrets_recovery_and_rollback(store: &NativeAuthStore) {
    // Secrets commits and fallback cleanup are separate resources, just as with
    // direct keyring storage. Resume that half state with the prepared digest.
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"stale-test\",\"extension\":true}",
    )
    .unwrap();
    let before_recovery = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after_recovery = snapshot(
        ClientKind::Codex,
        codex_auth("c", "org-c", "refresh-c"),
        serde_json::json!({}),
    );
    store
        .write_secrets_auth(
            &store.read_state().unwrap(),
            &after_recovery.auth.to_string(),
        )
        .unwrap();
    store
        .recover_apply(&before_recovery, &after_recovery, &prepared)
        .unwrap();
    assert_eq!(store.capture().unwrap().auth, after_recovery.auth);
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&store.auth_path).unwrap()).unwrap(),
        serde_json::json!({"extension":true})
    );
    fs::write(
        &store.auth_path,
        "{\"OPENAI_API_KEY\":\"stale-test\",\"extension\":true}",
    )
    .unwrap();
    let (before_rollback, prepared, source) = store.capture_for_switch().unwrap();
    assert_eq!(source, "secrets");
    let empty = snapshot(
        ClientKind::Codex,
        serde_json::json!({}),
        serde_json::json!({}),
    );
    store
        .write_secrets_auth(&store.read_state().unwrap(), "{}")
        .unwrap();
    store
        .recover_rollback(&before_rollback, &empty, &prepared, &source)
        .unwrap();
    assert_eq!(store.capture().unwrap().auth, before_rollback.auth);
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&store.auth_path).unwrap()).unwrap(),
        serde_json::json!({"extension":true})
    );
}

#[test]
fn codex_ephemeral_and_changed_backend_are_rejected() {
    let fixture = Fixture::new();
    fs::write(
        fixture.home.join("config.toml"),
        "cli_auth_credentials_store = \"ephemeral\"\n",
    )
    .unwrap();
    assert!(
        NativeAuthStore::with_credentials(
            ClientKind::Codex,
            &fixture.home.join("config.toml"),
            true,
            fixture.credentials.clone()
        )
        .is_err()
    );
    let store = fixture.codex("");
    let hash = store.fingerprint().unwrap();
    fs::write(
        fixture.home.join("config.toml"),
        "cli_auth_credentials_store = \"keyring\"\n",
    )
    .unwrap();
    let next = snapshot(
        ClientKind::Codex,
        serde_json::json!({}),
        serde_json::json!({}),
    );
    assert!(matches!(
        store.apply(&next, &hash),
        Err(DaemonError::Conflict(_))
    ));
}

#[test]
fn claude_file_switch_preserves_credentials_and_global_metadata_bytes() {
    let fixture = Fixture::new();
    let store = fixture.claude(false);
    fs::write(&store.auth_path, "{\n  \"otherSecret\": {\"keep\":true},\n  \"claudeAiOauth\":{\"accessToken\":\"test-a\"}\n}").unwrap();
    let metadata_path = store.metadata_path.as_ref().unwrap();
    fs::write(metadata_path, "{\r\n  // retain comment\r\n  \"projects\":{\"心\": {\"trusted\":true}},\r\n  \"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"}\r\n}\r\n").unwrap();
    let next = snapshot(
        ClientKind::Claude,
        serde_json::json!({"claudeAiOauth":{"accessToken":"test-b","refreshToken":"refresh-b","scopes":["user:inference"],"clientId":"test-client"}}),
        serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b","emailAddress":"b@example.test","displayName":"B"}}),
    );
    store.apply(&next, &store.fingerprint().unwrap()).unwrap();
    assert!(
        fs::read_to_string(&store.auth_path)
            .unwrap()
            .contains("  \"otherSecret\": {\"keep\":true},\n")
    );
    assert!(
        fs::read_to_string(metadata_path)
            .unwrap()
            .contains("  // retain comment\r\n  \"projects\":{\"心\": {\"trusted\":true}},\r\n")
    );
    let captured = store.capture().unwrap();
    assert_eq!(captured.identity.as_ref().unwrap().account_id, "b");
    assert_eq!(captured.auth["claudeAiOauth"]["clientId"], "test-client");
}

#[test]
fn claude_keychain_never_touches_plaintext_fallback() {
    let fixture = Fixture::new();
    let store = fixture.claude(true);
    let Backend::ClaudeKeychain { service, account } = &store.backend else {
        unreachable!()
    };
    fixture
        .credentials
        .store(
            service,
            account,
            "{\"otherSecret\":\"retain\",\"claudeAiOauth\":{\"accessToken\":\"old-test\"}}",
        )
        .unwrap();
    fs::write(&store.auth_path, "do not read this fallback").unwrap();
    let next = snapshot(
        ClientKind::Claude,
        serde_json::json!({"claudeAiOauth":{"accessToken":"new-test"}}),
        serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b"}}),
    );
    store.apply(&next, &store.fingerprint().unwrap()).unwrap();
    assert!(
        fixture
            .credentials
            .load(service, account)
            .unwrap()
            .unwrap()
            .contains("\"otherSecret\":\"retain\"")
    );
    assert_eq!(
        fs::read_to_string(&store.auth_path).unwrap(),
        "do not read this fallback"
    );
}

#[test]
fn snapshot_rejects_unowned_fields_and_wrong_client() {
    let fixture = Fixture::new();
    let store = fixture.codex("");
    let hash = store.fingerprint().unwrap();
    let polluted = snapshot(
        ClientKind::Codex,
        serde_json::json!({"mcpServers":{"changed":true}}),
        serde_json::json!({}),
    );
    assert!(matches!(
        store.apply(&polluted, &hash),
        Err(DaemonError::Invalid(_))
    ));
    let wrong_client = snapshot(
        ClientKind::Claude,
        serde_json::json!({}),
        serde_json::json!({}),
    );
    assert!(matches!(
        store.apply(&wrong_client, &hash),
        Err(DaemonError::Invalid(_))
    ));
    assert!(!store.auth_path.exists());
}

#[test]
fn separate_store_guards_conflict_and_nested_guard_is_safe() {
    let fixture = Fixture::new();
    let first = fixture.codex("");
    let second = fixture.codex("");
    let guard = first.lock_for_switch().unwrap();
    assert!(matches!(
        second.lock_for_switch(),
        Err(DaemonError::Conflict(_))
    ));
    let nested = first.lock_for_switch().unwrap();
    drop(nested);
    drop(guard);
    assert!(second.lock_for_switch().is_ok());
}

#[test]
fn claude_service_explicit_default_and_unicode_match_native_rule() {
    let home = Path::new("/test/claude-e\u{301}");
    assert_eq!(claude_service(home, false), "Claude Code-credentials");
    let normalized = "/test/claude-é";
    assert_eq!(
        claude_service(home, true),
        format!(
            "Claude Code-credentials-{}",
            &digest(normalized.as_bytes())[..8]
        )
    );
    assert_ne!(claude_service(home, true), claude_service(home, false));
}

#[test]
fn claude_native_home_uses_nfc_before_resolving_paths_and_legacy_lock() {
    let fixture = Fixture::new();
    let normalized = fixture.home.join("claude-é");
    fs::create_dir(&normalized).unwrap();
    let raw_configuration = fixture.home.join("claude-e\u{301}/settings.json");
    let store = NativeAuthStore::with_credentials(
        ClientKind::Claude,
        &raw_configuration,
        true,
        fixture.credentials.clone(),
    )
    .unwrap();
    assert_eq!(store.home, normalized);
    assert_eq!(store.auth_path, normalized.join(".credentials.json"));
    let guard = store.lock_for_switch().unwrap();
    assert!(normalized.join(".oauth_refresh.lock").is_dir());
    let mut legacy = normalized.canonicalize().unwrap().into_os_string();
    legacy.push(".lock");
    assert!(PathBuf::from(legacy.clone()).is_dir());
    drop(guard);
    assert!(!PathBuf::from(legacy).exists());
}

#[test]
fn claude_recovery_preserves_external_login_metadata() {
    let fixture = Fixture::new();
    let store = fixture.claude(false);
    fs::write(
        &store.auth_path,
        "{\"claudeAiOauth\":{\"accessToken\":\"a\"}}",
    )
    .unwrap();
    let metadata_path = store.metadata_path.as_ref().unwrap();
    fs::write(
        metadata_path,
        "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"}}",
    )
    .unwrap();
    let before = store.capture().unwrap();
    let prepared = store.fingerprint().unwrap();
    let after = snapshot(
        ClientKind::Claude,
        serde_json::json!({"claudeAiOauth":{"accessToken":"b"}}),
        serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b"}}),
    );
    fs::write(&store.auth_path, after.auth.to_string()).unwrap();
    let external =
        "{\"oauthAccount\":{\"accountUuid\":\"external\",\"organizationUuid\":\"external-org\"}}";
    fs::write(metadata_path, external).unwrap();
    assert!(matches!(
        store.recover_apply(&before, &after, &prepared),
        Err(DaemonError::Conflict(_))
    ));
    assert_eq!(fs::read_to_string(metadata_path).unwrap(), external);
    assert_eq!(store.capture().unwrap().auth, after.auth);
}

#[test]
fn cleanup_requires_login_marker_and_preserves_shared_secret_key() {
    let fixture = Fixture::new();
    let account = codex_account(&fixture.home, "secrets");
    let direct = codex_account(&fixture.home, "cli");
    fixture
        .credentials
        .store(CODEX_AUTH_SERVICE, &direct, "test-direct")
        .unwrap();
    fixture
        .credentials
        .store(CODEX_SECRETS_SERVICE, &account, "test-shared")
        .unwrap();
    assert!(
        cleanup_login_home_with_credentials(
            ClientKind::Codex,
            &fixture.home,
            fixture.credentials.as_ref()
        )
        .is_err()
    );
    fs::write(
        fixture.home.join(".hsin-official-login"),
        "{\"client\":\"codex\",\"login_id\":\"00000000-0000-4000-8000-000000000001\"}",
    )
    .unwrap();
    fs::create_dir_all(fixture.home.join("secrets")).unwrap();
    fs::write(fixture.home.join("secrets/mcp_oauth.age"), "test-mcp").unwrap();
    cleanup_login_home_with_credentials(
        ClientKind::Codex,
        &fixture.home,
        fixture.credentials.as_ref(),
    )
    .unwrap();
    assert!(
        fixture
            .credentials
            .load(CODEX_AUTH_SERVICE, &direct)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .credentials
            .load(CODEX_SECRETS_SERVICE, &account)
            .unwrap()
            .as_deref(),
        Some("test-shared")
    );
}

#[test]
fn json_patch_preserves_comments_and_removes_first_middle_last_owned_keys() {
    for before in [
        "{\"tokens\":{},/* keep */\"other\":true}",
        "{\"other\":true,/* keep */\"tokens\":{}}",
        "{\"first\":1,\"tokens\":{},/* keep */\"other\":true}",
        "{\"tokens\":{},/* keep */}",
    ] {
        let after = json_patch::patch(before, &serde_json::json!({}), CODEX_KEYS).unwrap();
        assert!(after.contains("/* keep */"));
        assert!(!parse_object(&after).unwrap().contains_key("tokens"));
    }
    assert!(
        json_patch::patch(
            "{\"tokens\":{},\"tokens\":{}}",
            &serde_json::json!({}),
            CODEX_KEYS
        )
        .is_err()
    );
}

#[test]
fn unreadable_secrets_key_never_replaces_ciphertext() {
    let fixture = Fixture::new();
    let store = fixture.codex_backend(
        "cli_auth_credentials_store = \"keyring\"\n",
        KeyringBackend::Secrets,
    );
    fs::create_dir_all(fixture.home.join("secrets")).unwrap();
    let ciphertext = b"existing-encrypted-auth-with-missing-key";
    fs::write(store.secrets_path(), ciphertext).unwrap();
    assert!(matches!(store.capture(), Err(DaemonError::Crypto)));
    assert_eq!(fs::read(store.secrets_path()).unwrap(), ciphertext);
    assert!(
        fixture
            .credentials
            .load(
                CODEX_SECRETS_SERVICE,
                &codex_account(&fixture.home, "secrets")
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn claude_metadata_cas_failure_leaves_external_metadata_intact() {
    struct ChangingStore {
        inner: Arc<MemoryNativeCredentialStore>,
        metadata_path: PathBuf,
    }
    impl NativeCredentialStore for ChangingStore {
        fn load(&self, service: &str, account: &str) -> Result<Option<String>> {
            self.inner.load(service, account)
        }
        fn store(&self, service: &str, account: &str, value: &str) -> Result<()> {
            self.inner.store(service, account, value)?;
            fs::write(
                &self.metadata_path,
                "{\"oauthAccount\":{\"accountUuid\":\"external\",\"organizationUuid\":\"external-org\"},\"projects\":{\"keep\":true}}",
            )?;
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.inner.delete(service, account)
        }
    }
    let fixture = Fixture::new();
    let mut store = fixture.claude(true);
    let Backend::ClaudeKeychain { service, account } = &store.backend else {
        unreachable!()
    };
    fixture
        .credentials
        .store(
            service,
            account,
            "{\"claudeAiOauth\":{\"accessToken\":\"test-a\"}}",
        )
        .unwrap();
    let path = store.metadata_path.as_ref().unwrap().clone();
    fs::write(&path, "{\"oauthAccount\":{\"accountUuid\":\"a\",\"organizationUuid\":\"org-a\"},\"projects\":{\"keep\":true}}").unwrap();
    store.credentials = Arc::new(ChangingStore {
        inner: fixture.credentials.clone(),
        metadata_path: path.clone(),
    });
    let next = snapshot(
        ClientKind::Claude,
        serde_json::json!({"claudeAiOauth":{"accessToken":"test-b"}}),
        serde_json::json!({"oauthAccount":{"accountUuid":"b","organizationUuid":"org-b"}}),
    );
    let fingerprint = store.fingerprint().unwrap();
    assert!(matches!(
        store.apply(&next, &fingerprint),
        Err(DaemonError::Conflict(_))
    ));
    let metadata: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(metadata["oauthAccount"]["accountUuid"], "external");
    assert_eq!(metadata["projects"], serde_json::json!({"keep":true}));
    assert_eq!(
        store.capture().unwrap().auth["claudeAiOauth"]["accessToken"],
        "test-b"
    );
}

#[cfg(unix)]
#[test]
fn cleanup_rejects_symlinked_login_home_and_marker() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let marker = "{\"client\":\"codex\",\"login_id\":\"00000000-0000-4000-8000-000000000001\"}";
    fs::write(fixture.home.join("marker-source"), marker).unwrap();
    symlink(
        fixture.home.join("marker-source"),
        fixture.home.join(".hsin-official-login"),
    )
    .unwrap();
    assert!(
        cleanup_login_home_with_credentials(
            ClientKind::Codex,
            &fixture.home,
            fixture.credentials.as_ref()
        )
        .is_err()
    );
    fs::remove_file(fixture.home.join(".hsin-official-login")).unwrap();
    fs::write(fixture.home.join(".hsin-official-login"), marker).unwrap();
    let alias = fixture.home.with_extension("alias");
    symlink(&fixture.home, &alias).unwrap();
    assert!(
        cleanup_login_home_with_credentials(
            ClientKind::Codex,
            &alias,
            fixture.credentials.as_ref()
        )
        .is_err()
    );
    fs::remove_file(alias).unwrap();
}
