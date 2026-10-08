use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn session(client: ClientKind) -> (LoginSession, mpsc::Receiver<Zeroizing<String>>) {
    let (input, incoming) = mpsc::channel(4);
    let (cancel, _) = watch::channel(false);
    let (done, _) = watch::channel(false);
    (
        LoginSession {
            status: RwLock::new(OfficialLoginStatus {
                login_id: uuid::Uuid::new_v4().to_string(),
                client,
                state: OfficialLoginState::Starting,
                browser_url: None,
                provider_id: None,
                error: None,
            }),
            cancel,
            done,
            input,
            cleanup_allowed: AtomicBool::new(true),
        },
        incoming,
    )
}

async fn request(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("fake CLI request");
    serde_json::from_str(&line).expect("fake CLI JSON request")
}

async fn response(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: Value) {
    writer
        .write_all(serde_json::to_string(&value).unwrap().as_bytes())
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
}

#[tokio::test]
async fn codex_transcript_initializes_and_checks_the_completed_account() {
    let (session, _) = session(ClientKind::Codex);
    let (client, server) = tokio::io::duplex(16 * 1024);
    let (reader, writer) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let fake = tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let initialize = request(&mut reader).await;
        assert_eq!(
            initialize.get("method").and_then(Value::as_str),
            Some("initialize")
        );
        assert_eq!(
            initialize
                .pointer("/params/clientInfo/name")
                .and_then(Value::as_str),
            Some("hsin")
        );
        response(
            &mut server_write,
            json!({"id":1,"result":{"userAgent":"fake-codex"}}),
        )
        .await;
        assert_eq!(
            request(&mut reader)
                .await
                .get("method")
                .and_then(Value::as_str),
            Some("initialized")
        );
        let start = request(&mut reader).await;
        assert_eq!(
            start.get("method").and_then(Value::as_str),
            Some("account/login/start")
        );
        assert_eq!(
            start.pointer("/params/type").and_then(Value::as_str),
            Some("chatgpt")
        );
        response(&mut server_write, json!({"id":2,"result":{"type":"chatgpt","loginId":"official-attempt","authUrl":"https://auth.openai.com/oauth/authorize?state=ephemeral"}})).await;
        // A completion from a different attempt must not complete this session.
        response(&mut server_write, json!({"method":"account/login/completed","params":{"loginId":"other-attempt","success":false,"error":"private-remote-error"}})).await;
        response(&mut server_write, json!({"method":"account/login/completed","params":{"loginId":"official-attempt","success":true}})).await;
        let read = request(&mut reader).await;
        assert_eq!(
            read.get("method").and_then(Value::as_str),
            Some("account/read")
        );
        assert_eq!(
            read.pointer("/params/refreshToken"),
            Some(&Value::Bool(false))
        );
        response(&mut server_write, json!({"id":3,"result":{"account":{"type":"chatgpt","email":"account@example.test"},"requiresOpenaiAuth":true}})).await;
    });
    transport::codex_login(writer, BufReader::new(reader), &session)
        .await
        .expect("valid transcript");
    fake.await.unwrap();
    assert_eq!(
        session.status.read().state,
        OfficialLoginState::AwaitingBrowser
    );
    assert!(session.status.read().browser_url.is_some());
    // Saving/enabling belongs to the daemon worker, never the protocol driver.
    assert!(session.status.read().provider_id.is_none());
}

#[tokio::test]
async fn codex_failure_does_not_return_remote_error_material() {
    let (session, _) = session(ClientKind::Codex);
    let (client, server) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let fake = tokio::spawn(async move {
        let _ = request(&mut BufReader::new(server_read)).await;
        response(
            &mut server_write,
            json!({"id":1,"error":{"code":-32603,"message":"secret-cli-token-and-url"}}),
        )
        .await;
    });
    let error = transport::codex_login(writer, BufReader::new(reader), &session)
        .await
        .expect_err("failure");
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert!(
        !error
            .args
            .values()
            .any(|value| value.contains("secret-cli-token-and-url"))
    );
    fake.await.unwrap();
}

#[tokio::test]
async fn claude_transcript_accepts_manual_code_with_a_prompt_without_newline() {
    let (session, incoming) = session(ClientKind::Claude);
    let (client, server) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(client);
    let (server_read, mut server_write) = tokio::io::split(server);
    let fake = tokio::spawn(async move {
        server_write.write_all(b"Opening browser to sign in...\nIf the browser didn't open, visit: \x1b]8;;https://claude.ai/oauth/authorize?state=temporary\x1b\\Sign in\x1b]8;;\x1b\\\nPaste code here if prompted > ").await.unwrap();
        let mut code = Zeroizing::new(String::new());
        BufReader::new(server_read)
            .read_line(&mut code)
            .await
            .unwrap();
        assert_eq!(code.trim(), "fake-code#fake-state");
        server_write
            .write_all(b"Login successful.\n")
            .await
            .unwrap();
    });
    session
        .input
        .try_send(Zeroizing::new("fake-code#fake-state".into()))
        .unwrap();
    transport::claude_login(writer, BufReader::new(reader), &session, incoming)
        .await
        .expect("valid transcript");
    fake.await.unwrap();
    assert_eq!(
        session.status.read().state,
        OfficialLoginState::AwaitingBrowser
    );
    assert!(
        session
            .status
            .read()
            .browser_url
            .as_ref()
            .is_some_and(|url| url.starts_with("https://claude.ai/"))
    );
}

#[tokio::test]
async fn claude_failure_keeps_cli_output_out_of_public_errors() {
    let (session, incoming) = session(ClientKind::Claude);
    let (client, mut server) = tokio::io::duplex(4096);
    server
        .write_all(b"Login failed: raw-sensitive-cli-error\n")
        .await
        .unwrap();
    drop(server);
    let (reader, writer) = tokio::io::split(client);
    let error = transport::claude_login(writer, BufReader::new(reader), &session, incoming)
        .await
        .expect_err("failure");
    assert_eq!(error.code, ErrorCode::AuthenticationFailed);
    assert!(
        !error
            .args
            .values()
            .any(|value| value.contains("raw-sensitive-cli-error"))
    );
}

#[test]
fn manual_codes_are_bounded_and_cannot_inject_additional_input_lines() {
    assert!(transport::valid_manual_code("code#state"));
    for input in [
        "code",
        "#state",
        "code#",
        "code#state#extra",
        "code#state\nextra",
        "code#state\r",
        "code#state\0",
    ] {
        assert!(!transport::valid_manual_code(input));
    }
    assert!(!transport::valid_manual_code(&format!(
        "{}#state",
        "x".repeat(16 * 1024)
    )));
}

#[test]
fn claude_capability_check_requires_the_pasted_code_support() {
    for version in [
        "2.1.126 (Claude Code)",
        "2.1.293 (Claude Code)",
        "2.2.0",
        "3.0.0",
    ] {
        assert!(transport::claude_supports_manual_code(version));
    }
    for version in [
        "",
        "not-a-version",
        "2.1.41",
        "2.1.125",
        "2.0.293",
        "1.99.999",
        "2.1.126-dev",
        "2.1.126.extra",
    ] {
        assert!(!transport::claude_supports_manual_code(version));
    }
}

#[test]
fn codex_version_floor_matches_official_version_output_and_rejects_old_or_unknown_versions() {
    for version in [
        "codex-cli 0.161.0",
        "codex-cli 0.161.1\n",
        "codex-cli 0.162.0",
        "codex-cli 1.0.0",
    ] {
        assert!(transport::meets_version_floor(
            ClientKind::Codex,
            version,
            (0, 161, 0)
        ));
    }
    for version in [
        "",
        "0.161.0",
        "codex 0.161.0",
        "codex-cli 0.160.999",
        "codex-cli 0.100.0",
        "codex-cli 0.161",
        "codex-cli 0.161.0.dev",
        "codex-cli 0.161.0-alpha",
        "codex-cli 0.161.0+build",
        "codex-cli 18446744073709551616.0.0",
    ] {
        assert!(!transport::meets_version_floor(
            ClientKind::Codex,
            version,
            (0, 161, 0)
        ));
    }
    assert!(!transport::meets_version_floor(
        ClientKind::Claude,
        "codex-cli 0.161.0",
        (2, 1, 126)
    ));
}

#[test]
fn native_compatibility_tests_use_the_mock_store_without_inspecting_installed_clients() {
    for client in ClientKind::ALL {
        ensure_native_account_compatibility(client).unwrap();
    }
}

#[test]
fn terminal_progress_discards_ephemeral_url() {
    let (session, _) = session(ClientKind::Codex);
    session.awaiting_browser("https://auth.openai.com/oauth/authorize?state=discard".into());
    session.finish(OfficialLoginState::Cancelled, None, None);
    assert!(!session.is_active());
    assert!(session.status.read().browser_url.is_none());
    assert!(*session.done.borrow());
}

#[test]
fn staging_marker_and_config_use_private_permissions() {
    let root =
        std::env::temp_dir().join(format!("hsin-login-permissions-{}", uuid::Uuid::new_v4()));
    let home = root.join(uuid::Uuid::new_v4().to_string());
    prepare_login_home(
        &home,
        ClientKind::Claude,
        home.file_name().unwrap().to_str().unwrap(),
    )
    .unwrap();
    write_private(&home.join("settings.json"), b"{}").unwrap();
    let marker: LoginMarker =
        serde_json::from_slice(&fs::read(home.join(MARKER)).unwrap()).unwrap();
    assert_eq!(marker.client, ClientKind::Claude);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(home.join(MARKER))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(home.join("settings.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn fake_cli_capability_check_runs_in_an_isolated_home() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("hsin-login-fake-cli-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let executable = root.join("fake-claude");
    fs::write(&executable, b"#!/bin/sh\n[ \"$PWD\" -ef \"$CLAUDE_CONFIG_DIR\" ] || exit 1\n[ \"$PWD\" -ef \"$CODEX_HOME\" ] || exit 1\nif [ \"$1\" = '--version' ]; then echo '2.1.293 (Claude Code)'; exit 0; fi\n[ \"$1 $2 $3\" = \"auth login --help\" ] || exit 1\necho 'Usage: auth login --claudeai'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let (session, _) = session(ClientKind::Claude);
    let mut cancellation = session.cancel.subscribe();
    transport::check_client_capability(
        ClientKind::Claude,
        &executable,
        &root,
        &BTreeMap::new(),
        &session,
        &mut cancellation,
    )
    .await
    .unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_never_terminates_a_reused_pid() {
    let marker = LoginMarker {
        client: ClientKind::Claude,
        login_id: uuid::Uuid::new_v4().to_string(),
        child_pid: Some(std::process::id()),
        child_start: Some("different-process-identity".into()),
        isolated_process_group: false,
        tree_terminated: false,
        spawn_pending: false,
    };
    process_identity::stop_marked_child(&marker, Path::new("/unused-login-home")).unwrap();
}

#[cfg(unix)]
#[derive(Default)]
struct MemoryKeys(Mutex<HashMap<u32, String>>);

#[cfg(unix)]
impl crate::crypto::KeyStore for MemoryKeys {
    fn load(&self, version: u32) -> Result<Option<String>> {
        Ok(self.0.lock().get(&version).cloned())
    }
    fn store(&self, version: u32, value: &str) -> Result<()> {
        self.0.lock().insert(version, value.into());
        Ok(())
    }
    fn delete(&self, version: u32) -> Result<()> {
        self.0.lock().remove(&version);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Default)]
struct FakeCredentials {
    deleted: Mutex<Vec<(String, String)>>,
}

#[cfg(unix)]
impl NativeCredentialStore for FakeCredentials {
    fn load(&self, service: &str, _account: &str) -> Result<Option<String>> {
        Ok(service
            .starts_with("Claude Code-credentials-")
            .then(|| fake_claude_credentials().to_string()))
    }
    fn store(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        Ok(())
    }
    fn delete(&self, service: &str, account: &str) -> Result<()> {
        self.deleted.lock().push((service.into(), account.into()));
        Ok(())
    }
}

#[cfg(unix)]
fn fake_claude_credentials() -> Value {
    json!({"claudeAiOauth":{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":4_000_000_000_000_u64,"scopes":["user:inference","user:profile"]}})
}

#[cfg(unix)]
struct Fixture {
    root: PathBuf,
    app: Arc<App>,
    credentials: Arc<FakeCredentials>,
}

#[cfg(unix)]
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("hsin-official-login-test-{}", uuid::Uuid::new_v4()));
        let paths = crate::paths::Paths::for_home(root.join("instance"));
        let mut app = App::open_with_store(&paths, Arc::new(MemoryKeys::default())).unwrap();
        let credentials = Arc::new(FakeCredentials::default());
        Arc::get_mut(&mut app).unwrap().native_credentials = credentials.clone();
        *app.config_paths.write() = HashMap::from([
            (ClientKind::Codex, root.join("codex/config.toml")),
            (ClientKind::Claude, root.join("claude/settings.json")),
        ]);
        for client in ClientKind::ALL {
            let configuration = app.config_path(client).unwrap();
            fs::create_dir_all(configuration.parent().unwrap()).unwrap();
            let value: &[u8] = match client {
                ClientKind::Codex => {
                    b"# current third-party configuration\nmodel_provider = \"third_party\"\n"
                }
                ClientKind::Claude => {
                    br#"{"env":{"ANTHROPIC_API_KEY":"third-party-test-key"},"hooks":{}}"#
                }
            };
            fs::write(configuration, value).unwrap();
        }
        Self {
            root,
            app,
            credentials,
        }
    }

    fn home(&self, login_id: &str) -> PathBuf {
        Path::new(&self.app.instance.instance_home)
            .join("official-login")
            .join(login_id)
    }

    #[cfg(unix)]
    fn executable(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let executable = self.root.join(format!("fake-cli-{}", uuid::Uuid::new_v4()));
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }

    fn install_session(
        &self,
        client: ClientKind,
    ) -> (
        Arc<LoginSession>,
        mpsc::Receiver<Zeroizing<String>>,
        watch::Receiver<bool>,
    ) {
        let (session, incoming) = session(client);
        let cancellation = session.cancel.subscribe();
        let session = Arc::new(session);
        let id = session.status.read().login_id.clone();
        self.app
            .official_logins
            .sessions
            .lock()
            .insert(id, session.clone());
        (session, incoming, cancellation)
    }
}

#[cfg(unix)]
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn fake_codex_cli_saves_a_complete_account_without_enabling_or_changing_native_config() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let fixture = Fixture::new();
    let before = fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap();
    let jwt = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"fake-user","email":"codex@example.test","https://api.openai.com/auth":{"chatgpt_user_id":"fake-user","chatgpt_account_id":"fake-workspace"}})).unwrap()));
    let auth = json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":"fake-access","refresh_token":"fake-refresh","account_id":"fake-workspace"}}).to_string();
    let executable = fixture.executable(&format!(r#"
if [ "$2" = '--help' ]; then echo 'app-server --listen'; exit 0; fi
read -r request
echo '{{"id":1,"result":{{"userAgent":"fake-cli"}}}}'
read -r initialized
read -r request
echo '{{"id":2,"result":{{"type":"chatgpt","loginId":"fake-attempt","authUrl":"https://auth.openai.com/oauth/authorize?state=fake"}}}}'
cat > "$CODEX_HOME/auth.json" <<'AUTH'
{auth}
AUTH
echo '{{"method":"account/login/completed","params":{{"loginId":"fake-attempt","success":true}}}}'
read -r request
echo '{{"id":3,"result":{{"account":{{"type":"chatgpt","email":"codex@example.test"}}}}}}'
while read -r request; do :; done
"#));
    let (session, incoming, cancellation) = fixture.install_session(ClientKind::Codex);
    let login_id = session.status.read().login_id.clone();
    fixture
        .app
        .clone()
        .run_official_login(
            session.clone(),
            executable,
            BTreeMap::new(),
            incoming,
            cancellation,
        )
        .await;
    let status = session.status.read().clone();
    assert_eq!(
        status.state,
        OfficialLoginState::Completed,
        "{:?}",
        status.error
    );
    assert!(status.browser_url.is_none());
    let saved = fixture
        .app
        .db
        .get_provider(status.provider_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        saved.official_account.as_ref().unwrap().email.as_deref(),
        Some("codex@example.test")
    );
    assert!(
        fixture
            .app
            .db
            .client_state(ClientKind::Codex)
            .unwrap()
            .active_provider_id
            .is_none()
    );
    assert_eq!(
        fs::read(fixture.app.config_path(ClientKind::Codex).unwrap()).unwrap(),
        before
    );
    assert!(!fixture.root.join("codex/auth.json").exists());
    assert!(!fixture.home(&login_id).exists());
    assert!(!fixture.credentials.deleted.lock().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn fake_claude_cli_saves_only_after_manual_login_and_cleans_mock_credentials() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.app.config_path(ClientKind::Claude).unwrap()).unwrap();
    let executable = fixture.executable(&format!(r#"
if [ "$1" = '--version' ]; then echo '2.1.293 (Claude Code)'; exit 0; fi
if [ "$3" = '--help' ]; then echo 'auth login --claudeai'; exit 0; fi
echo 'If the browser did not open, visit: https://claude.ai/oauth/authorize?state=fake'
printf 'Paste code > '
read -r code
[ "$code" = 'fake-code#fake-state' ] || exit 1
cat > "$CLAUDE_CONFIG_DIR/.credentials.json" <<'AUTH'
{}
AUTH
cat > "$CLAUDE_CONFIG_DIR/.claude.json" <<'META'
{{"oauthAccount":{{"accountUuid":"fake-user","organizationUuid":"fake-organization","emailAddress":"claude@example.test","displayName":"Example Claude"}}}}
META
echo 'Login successful.'
"#, fake_claude_credentials()));
    let (session, incoming, cancellation) = fixture.install_session(ClientKind::Claude);
    let login_id = session.status.read().login_id.clone();
    let worker = tokio::spawn(fixture.app.clone().run_official_login(
        session.clone(),
        executable,
        BTreeMap::new(),
        incoming,
        cancellation,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.status.read().state != OfficialLoginState::AwaitingBrowser {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .app
            .db
            .list_providers(Some(ClientKind::Claude))
            .unwrap()
            .is_empty()
    );
    fixture
        .app
        .submit_official_login(OfficialLoginSubmitParams {
            login_id: login_id.clone(),
            code: "fake-code#fake-state".into(),
        })
        .await
        .unwrap();
    worker.await.unwrap();
    let status = session.status.read().clone();
    assert_eq!(
        status.state,
        OfficialLoginState::Completed,
        "{:?}",
        status.error
    );
    let saved = fixture
        .app
        .db
        .get_provider(status.provider_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        saved.official_account.as_ref().unwrap().email.as_deref(),
        Some("claude@example.test")
    );
    assert!(
        fixture
            .app
            .db
            .client_state(ClientKind::Claude)
            .unwrap()
            .active_provider_id
            .is_none()
    );
    assert_eq!(
        fs::read(fixture.app.config_path(ClientKind::Claude).unwrap()).unwrap(),
        before
    );
    assert!(!fixture.home(&login_id).exists());
    #[cfg(target_os = "macos")]
    assert!(
        fixture
            .credentials
            .deleted
            .lock()
            .iter()
            .any(|(service, _)| service.starts_with("Claude Code-credentials-"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_the_wrapper_and_its_delayed_credential_writer() {
    let fixture = Fixture::new();
    let executable = fixture.executable(r#"
if [ "$2" = '--help' ]; then echo 'app-server --listen'; exit 0; fi
read -r request
echo '{"id":1,"result":{"userAgent":"fake-cli"}}'
read -r initialized
read -r request
( sleep 2; mkdir -p "$CODEX_HOME"; echo 'unexpected late credentials' > "$CODEX_HOME/auth.json" ) &
echo $! > "$CODEX_HOME/descendant.pid"
echo '{"id":2,"result":{"type":"chatgpt","loginId":"fake-attempt","authUrl":"https://auth.openai.com/oauth/authorize?state=fake"}}'
while read -r request; do :; done
"#);
    let (session, incoming, cancellation) = fixture.install_session(ClientKind::Codex);
    let login_id = session.status.read().login_id.clone();
    let worker = tokio::spawn(fixture.app.clone().run_official_login(
        session.clone(),
        executable,
        BTreeMap::new(),
        incoming,
        cancellation,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.status.read().state != OfficialLoginState::AwaitingBrowser {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let marker: LoginMarker =
        serde_json::from_slice(&fs::read(fixture.home(&login_id).join(MARKER)).unwrap()).unwrap();
    let status = fixture
        .app
        .cancel_official_login(OfficialLoginStatusParams {
            login_id: login_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(status.state, OfficialLoginState::Cancelled);
    assert!(status.browser_url.is_none());
    assert!(process_identity::process_start(marker.child_pid.unwrap()).is_err());
    assert!(!fixture.home(&login_id).exists());
    worker.await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !fixture.home(&login_id).exists(),
        "a surviving descendant recreated the login home"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_during_capability_probe_kills_wrapper_descendants_before_cleanup() {
    let fixture = Fixture::new();
    let executable = fixture.executable(r#"
if [ "$2" = '--help' ]; then
    ( sleep 2; mkdir -p "$CODEX_HOME"; echo 'unexpected probe credentials' > "$CODEX_HOME/auth.json" ) &
    echo ready > "$CODEX_HOME/probe-ready"
    echo 'app-server --listen'
    wait
    exit 0
fi
exit 1
"#);
    let (session, incoming, cancellation) = fixture.install_session(ClientKind::Codex);
    let login_id = session.status.read().login_id.clone();
    let worker = tokio::spawn(fixture.app.clone().run_official_login(
        session.clone(),
        executable,
        BTreeMap::new(),
        incoming,
        cancellation,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.home(&login_id).join("probe-ready").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = fixture
        .app
        .cancel_official_login(OfficialLoginStatusParams {
            login_id: login_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(status.state, OfficialLoginState::Cancelled);
    worker.await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !fixture.home(&login_id).exists(),
        "a surviving capability descendant recreated the login home"
    );
    assert!(
        fixture
            .app
            .db
            .client_state(ClientKind::Codex)
            .unwrap()
            .active_provider_id
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn recovery_stops_a_marked_child_and_leaves_unmarked_directories() {
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    let login_id = uuid::Uuid::new_v4().to_string();
    let home = fixture.home(&login_id);
    prepare_login_home(&home, ClientKind::Codex, &login_id).unwrap();
    let home = fs::canonicalize(home).unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "while :; do read -r input; done"])
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    write_marker(
        &home,
        &LoginMarker {
            client: ClientKind::Codex,
            login_id,
            child_pid: Some(pid),
            child_start: Some(process_identity::process_start(pid).unwrap()),
            isolated_process_group: true,
            tree_terminated: false,
            spawn_pending: false,
        },
    )
    .unwrap();
    let reaper = std::thread::spawn(move || child.wait().unwrap());
    let unmarked = home
        .parent()
        .unwrap()
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("keep"), b"unmarked").unwrap();
    fixture.app.cleanup_official_logins().unwrap();
    reaper.join().unwrap();
    assert!(!home.exists());
    assert!(unmarked.join("keep").exists());
}

#[cfg(unix)]
#[test]
fn recovery_retains_a_login_home_when_spawn_identity_was_not_committed() {
    let fixture = Fixture::new();
    let (session, _) = session(ClientKind::Codex);
    let home = fixture.home(&session.status.read().login_id);
    prepare_login_home(&home, ClientKind::Codex, &session.status.read().login_id).unwrap();
    record_child_marker(&session, &home, None).unwrap();
    assert!(fixture.app.cleanup_official_logins().is_err());
    assert!(home.join(MARKER).exists());
    assert!(fixture.credentials.deleted.lock().is_empty());
}

#[cfg(unix)]
#[test]
fn recovery_retains_a_live_group_whose_creation_identity_does_not_match() {
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    let login_id = uuid::Uuid::new_v4().to_string();
    let home = fixture.home(&login_id);
    prepare_login_home(&home, ClientKind::Codex, &login_id).unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "while :; do read -r input; done"])
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    write_marker(
        &home,
        &LoginMarker {
            client: ClientKind::Codex,
            login_id,
            child_pid: Some(child.id()),
            child_start: Some("different-creation-identity".into()),
            isolated_process_group: true,
            tree_terminated: false,
            spawn_pending: false,
        },
    )
    .unwrap();
    assert!(fixture.app.cleanup_official_logins().is_err());
    assert!(home.join(MARKER).exists());
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
}
