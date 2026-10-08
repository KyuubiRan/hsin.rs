//! Isolated official-client login sessions. Only public progress crosses IPC.
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use hsin_core::{
    AppError, ClientKind, ErrorCode, OfficialLoginStartParams, OfficialLoginState,
    OfficialLoginStatus, OfficialLoginStatusParams, OfficialLoginSubmitParams, UpstreamProxyMode,
};
use parking_lot::{Mutex, RwLock};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};
use zeroize::Zeroizing;

use super::App;
use crate::{
    error::{DaemonError, Result},
    native_auth::{NativeAuthStore, NativeCredentialStore, cleanup_login_home_with_credentials},
};

mod process_identity;
#[cfg(test)]
mod tests;
mod transport;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MARKER: &str = ".hsin-official-login";
const MAX_RETAINED_LOGINS: usize = 32;

pub(super) fn ensure_native_account_compatibility(client: ClientKind) -> Result<()> {
    transport::ensure_native_account_compatibility(client)
}

#[derive(Default)]
pub(super) struct LoginManager {
    sessions: Mutex<HashMap<String, Arc<LoginSession>>>,
}

struct LoginSession {
    status: RwLock<OfficialLoginStatus>,
    cancel: watch::Sender<bool>,
    done: watch::Sender<bool>,
    input: mpsc::Sender<Zeroizing<String>>,
    cleanup_allowed: AtomicBool,
}

impl LoginSession {
    fn is_active(&self) -> bool {
        matches!(
            self.status.read().state,
            OfficialLoginState::Starting | OfficialLoginState::AwaitingBrowser
        )
    }

    fn awaiting_browser(&self, url: String) {
        let mut status = self.status.write();
        status.state = OfficialLoginState::AwaitingBrowser;
        status.browser_url = Some(url);
    }

    fn finish(
        &self,
        state: OfficialLoginState,
        provider_id: Option<String>,
        error: Option<AppError>,
    ) {
        let mut status = self.status.write();
        status.state = state;
        status.browser_url = None;
        status.provider_id = provider_id;
        status.error = error;
        self.done.send_replace(true);
    }
}

#[derive(Serialize, Deserialize)]
struct LoginMarker {
    client: ClientKind,
    login_id: String,
    #[serde(default)]
    child_pid: Option<u32>,
    #[serde(default)]
    child_start: Option<String>,
    #[serde(default)]
    isolated_process_group: bool,
    #[serde(default)]
    tree_terminated: bool,
    #[serde(default)]
    spawn_pending: bool,
}

impl App {
    pub async fn start_official_login(
        self: &Arc<Self>,
        params: OfficialLoginStartParams,
    ) -> Result<OfficialLoginStatus> {
        if !self.crypto.is_unlocked() {
            return Err(DaemonError::Locked);
        }
        let environment = self.official_login_environment().await?;
        let executable = find_client_executable(params.client)?;
        let login_id = uuid::Uuid::new_v4().to_string();
        let (cancel, cancellation) = watch::channel(false);
        let (done, _) = watch::channel(false);
        let (input, incoming) = mpsc::channel(4);
        let status = OfficialLoginStatus {
            login_id: login_id.clone(),
            client: params.client,
            state: OfficialLoginState::Starting,
            browser_url: None,
            provider_id: None,
            error: None,
        };
        let session = Arc::new(LoginSession {
            status: RwLock::new(status.clone()),
            cancel,
            done,
            input,
            cleanup_allowed: AtomicBool::new(true),
        });
        {
            let mut sessions = self.official_logins.sessions.lock();
            if sessions
                .values()
                .any(|session| session.is_active() && session.status.read().client == params.client)
            {
                return Err(DaemonError::Conflict(
                    "an official login is already in progress for this client".into(),
                ));
            }
            if sessions.len() >= MAX_RETAINED_LOGINS {
                sessions.retain(|_, session| session.is_active());
            }
            sessions.insert(login_id, session.clone());
        }
        let app = self.clone();
        tokio::spawn(async move {
            app.run_official_login(session, executable, environment, incoming, cancellation)
                .await;
        });
        Ok(status)
    }

    pub fn official_login_status(
        &self,
        params: OfficialLoginStatusParams,
    ) -> Result<OfficialLoginStatus> {
        let login_id = params.login_id;
        let session = self.login_session(&login_id)?;
        let status = session.status.read().clone();
        Ok(status)
    }

    pub async fn submit_official_login(
        &self,
        params: OfficialLoginSubmitParams,
    ) -> Result<OfficialLoginStatus> {
        let code = Zeroizing::new(params.code);
        let session = self.login_session(&params.login_id)?;
        {
            let status = session.status.read();
            if status.client != ClientKind::Claude
                || status.state != OfficialLoginState::AwaitingBrowser
            {
                return Err(DaemonError::Invalid(
                    "this login does not accept a pasted authorization code".into(),
                ));
            }
        }
        if !transport::valid_manual_code(&code) {
            return Err(DaemonError::Invalid(
                "paste the complete authorization code and state without line breaks".into(),
            ));
        }
        session
            .input
            .send_timeout(code, Duration::from_secs(1))
            .await
            .map_err(|_| DaemonError::Conflict("the login is busy or has finished".into()))?;
        let status = session.status.read().clone();
        Ok(status)
    }

    pub async fn cancel_official_login(
        &self,
        params: OfficialLoginStatusParams,
    ) -> Result<OfficialLoginStatus> {
        let session = self.login_session(&params.login_id)?;
        let mut done = session.done.subscribe();
        if session.is_active() {
            session.cancel.send_replace(true);
            // The worker kills and reaps the child before exposing cancellation.
            tokio::time::timeout(Duration::from_secs(20), async {
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(|_| {
                DaemonError::Conflict("official login cleanup is still in progress".into())
            })?;
        }
        let status = session.status.read().clone();
        Ok(status)
    }

    fn login_session(&self, login_id: &str) -> Result<Arc<LoginSession>> {
        self.official_logins
            .sessions
            .lock()
            .get(login_id)
            .cloned()
            .ok_or_else(|| DaemonError::NotFound("official login session".into()))
    }

    /// At daemon startup, only marked, isolated login homes are eligible for cleanup.
    pub(crate) fn cleanup_official_logins(&self) -> Result<()> {
        let root = Path::new(&self.instance.instance_home).join("official-login");
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let home = entry.path();
            let marker_path = home.join(MARKER);
            match fs::symlink_metadata(&marker_path) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= 4096 => {}
                _ => continue,
            }
            let marker: LoginMarker = match serde_json::from_slice(&fs::read(&marker_path)?) {
                Ok(marker) => marker,
                Err(_) => continue,
            };
            if uuid::Uuid::parse_str(&marker.login_id).is_err()
                || entry.file_name() != OsString::from(&marker.login_id)
                || self
                    .official_logins
                    .sessions
                    .lock()
                    .get(&marker.login_id)
                    .is_some_and(|session| session.is_active())
            {
                continue;
            }
            process_identity::stop_marked_child(&marker, &home)?;
            clean_home(marker.client, &home, &*self.native_credentials)?;
        }
        Ok(())
    }

    pub(crate) async fn cancel_all_official_logins(&self) {
        let sessions = self
            .official_logins
            .sessions
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in &sessions {
            if session.is_active() {
                session.cancel.send_replace(true);
            }
        }
        for session in sessions {
            let mut done = session.done.subscribe();
            let _ = tokio::time::timeout(Duration::from_secs(20), async {
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }

    async fn run_official_login(
        self: Arc<Self>,
        session: Arc<LoginSession>,
        executable: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        incoming: mpsc::Receiver<Zeroizing<String>>,
        mut cancellation: watch::Receiver<bool>,
    ) {
        let status = session.status.read().clone();
        let home = Path::new(&self.instance.instance_home)
            .join("official-login")
            .join(&status.login_id);
        let prepared = prepare_login_home(&home, status.client, &status.login_id)
            .and_then(|()| self.copy_official_login_policy(status.client, &home));
        let outcome = if let Err(error) = prepared {
            Err(login_error(&error))
        } else if *cancellation.borrow() {
            Ok(None)
        } else {
            self.login_child(
                &session,
                &executable,
                &home,
                &environment,
                incoming,
                &mut cancellation,
            )
            .await
        };
        // Cleanup errors never erase the marker; startup retries the isolated removal.
        let cleaned = if session.cleanup_allowed.load(Ordering::Acquire) {
            clean_home(status.client, &home, &*self.native_credentials)
        } else {
            Err(DaemonError::Config(
                "cannot verify that the isolated login process tree has exited".into(),
            ))
        };
        match (outcome, cleaned) {
            (Ok(Some(provider_id)), Ok(())) => {
                session.finish(OfficialLoginState::Completed, Some(provider_id), None);
            }
            (Ok(None), Ok(())) => session.finish(OfficialLoginState::Cancelled, None, None),
            (Err(error), _) => session.finish(OfficialLoginState::Failed, None, Some(error)),
            (Ok(provider_id), Err(error)) => session.finish(
                OfficialLoginState::Failed,
                provider_id,
                Some(login_error(&error)),
            ),
        }
    }

    async fn login_child(
        self: &Arc<Self>,
        session: &Arc<LoginSession>,
        executable: &Path,
        home: &Path,
        environment: &BTreeMap<OsString, OsString>,
        incoming: mpsc::Receiver<Zeroizing<String>>,
        cancellation: &mut watch::Receiver<bool>,
    ) -> std::result::Result<Option<String>, AppError> {
        let client = session.status.read().client;
        let mut command = login_command(client, executable, home, environment);
        let checked = transport::check_client_capability(
            client,
            executable,
            home,
            environment,
            session,
            cancellation,
        )
        .await;
        if *cancellation.borrow() {
            return Ok(None);
        }
        checked?;
        record_child_marker(session, home, None)?;
        let mut child = command.spawn().map_err(|_| {
            failure(
                ErrorCode::ConfigUnavailable,
                "cannot launch the official client; check its installation",
            )
        })?;
        session.cleanup_allowed.store(false, Ordering::Release);
        let child_pid = child.id().ok_or_else(|| {
            failure(
                ErrorCode::Internal,
                "cannot identify the official login process",
            )
        })?;
        let recorded = record_child_marker(session, home, Some(child_pid));
        if recorded.is_err() {
            let _ =
                terminate_login_child(&mut child, child_pid, home, &session.cleanup_allowed).await;
            return Err(failure(
                ErrorCode::ConfigUnavailable,
                "cannot record the isolated login process for recovery",
            ));
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| failure(ErrorCode::Internal, "official login input is unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| failure(ErrorCode::Internal, "official login output is unavailable"))?;
        let protocol = transport::login(client, stdin, stdout, session, incoming);
        let result = tokio::select! {
            result = tokio::time::timeout(LOGIN_TIMEOUT, protocol) => result.map_err(|_| failure(ErrorCode::Timeout, "official login timed out; try again")).and_then(|value| value),
            _ = cancellation.changed() => {
                terminate_login_child(&mut child, child_pid, home, &session.cleanup_allowed).await?;
                return Ok(None);
            }
        };
        if result.is_ok() && client == ClientKind::Claude {
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) if status.success() => {}
                _ => {
                    terminate_login_child(&mut child, child_pid, home, &session.cleanup_allowed)
                        .await?;
                    return Err(failure(
                        ErrorCode::AuthenticationFailed,
                        "official client login failed; check the client and try again",
                    ));
                }
            }
        }
        terminate_login_child(&mut child, child_pid, home, &session.cleanup_allowed).await?;
        result?;
        if *cancellation.borrow() {
            return Ok(None);
        }
        let snapshot = NativeAuthStore::with_credentials(
            client,
            &home.join(config_file(client)),
            true,
            self.native_credentials.clone(),
        )
        .map_err(|error| login_error(&error))?
        .capture()
        .map_err(|error| login_error(&error))?;
        if snapshot.identity.is_none() {
            return Err(failure(
                ErrorCode::AuthenticationFailed,
                "official login did not produce a complete account",
            ));
        }
        let provider = self
            .save_official_account(&snapshot, false)
            .await
            .map_err(|error| login_error(&error))?;
        Ok(Some(provider.id))
    }

    fn copy_official_login_policy(&self, client: ClientKind, home: &Path) -> Result<()> {
        let source = self.config_path(client)?;
        let text = match fs::read_to_string(source) {
            Ok(text) => Zeroizing::new(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let output = match client {
            ClientKind::Codex => {
                let document: toml_edit::DocumentMut = text.parse().map_err(|_| {
                    DaemonError::Config("cannot read Codex login restrictions".into())
                })?;
                let mut policy = toml_edit::DocumentMut::new();
                for key in ["forced_login_method", "forced_chatgpt_workspace_id"] {
                    if let Some(item) = document.get(key) {
                        policy[key] = item.clone();
                    }
                }
                if policy
                    .get("forced_login_method")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(|method| method != "chatgpt")
                {
                    return Err(DaemonError::PermissionDenied(
                        "Codex configuration restricts the login method".into(),
                    ));
                }
                policy.to_string()
            }
            ClientKind::Claude => {
                let value = jsonc_parser::parse_to_serde_value(
                    &text,
                    &jsonc_parser::ParseOptions::default(),
                )
                .map_err(|_| {
                    DaemonError::Config("cannot read Claude Code login restrictions".into())
                })?
                .unwrap_or(serde_json::Value::Null);
                let mut policy = serde_json::Map::new();
                for key in ["forceLoginMethod", "forceLoginOrgUUID", "allowedProviders"] {
                    if let Some(value) = value.get(key) {
                        policy.insert(key.into(), value.clone());
                    }
                }
                if policy
                    .get("forceLoginMethod")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|method| method != "claudeai")
                {
                    return Err(DaemonError::PermissionDenied(
                        "Claude Code configuration restricts the login method".into(),
                    ));
                }
                serde_json::to_string(&policy)?
            }
        };
        write_private(&home.join(config_file(client)), output.as_bytes())
    }

    async fn official_login_environment(&self) -> Result<BTreeMap<OsString, OsString>> {
        let proxy = self.upstream_outbound_proxy()?;
        let mut environment = BTreeMap::new();
        let endpoint = match proxy.config.mode {
            UpstreamProxyMode::Direct => Some(String::new()),
            UpstreamProxyMode::Manual => {
                let manual = &proxy.config.manual;
                let host = if manual.host.contains(':') {
                    format!("[{}]", manual.host)
                } else {
                    manual.host.clone()
                };
                let mut endpoint = url::Url::parse(&format!(
                    "{}://{host}:{}",
                    manual.protocol.as_str(),
                    manual.port
                ))
                .map_err(|_| DaemonError::Config("invalid official login proxy".into()))?;
                if !manual.username.is_empty() {
                    endpoint.set_username(&manual.username).map_err(|()| {
                        DaemonError::Config("invalid official login proxy username".into())
                    })?;
                    endpoint
                        .set_password(proxy.password.as_ref().map(ExposeSecret::expose_secret))
                        .map_err(|()| {
                            DaemonError::Config("invalid official login proxy password".into())
                        })?;
                }
                Some(endpoint.to_string())
            }
            UpstreamProxyMode::System => {
                if [
                    "http_proxy",
                    "https_proxy",
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "all_proxy",
                    "ALL_PROXY",
                ]
                .iter()
                .any(|key| std::env::var_os(key).is_some())
                {
                    None
                } else {
                    tokio::task::spawn_blocking(|| {
                        systemproxy::SystemProxy::get_system_proxy().ok()
                    })
                    .await
                    .ok()
                    .flatten()
                    .filter(|proxy| {
                        proxy.enable && !proxy.host.trim().is_empty() && proxy.port != 0
                    })
                    .map(|proxy| format!("http://{}:{}", proxy.host, proxy.port))
                }
            }
        };
        if let Some(endpoint) = endpoint {
            for key in [
                "http_proxy",
                "https_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "all_proxy",
                "ALL_PROXY",
            ] {
                environment.insert(OsString::from(key), OsString::from(&endpoint));
            }
        }
        let bypass = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .unwrap_or_default();
        let bypass = format!("localhost,127.0.0.1,::1,{bypass}");
        environment.insert("NO_PROXY".into(), bypass.clone().into());
        environment.insert("no_proxy".into(), bypass.into());
        Ok(environment)
    }
}

fn failure(code: ErrorCode, message: &str) -> AppError {
    AppError::new(code).with_arg("message", message)
}

fn login_error(error: &DaemonError) -> AppError {
    let code = AppError::from(error).code;
    failure(
        code,
        match code {
            ErrorCode::KeyStoreLocked | ErrorCode::KeyStoreUnavailable => {
                "unlock hsin's credential storage before adding an official account"
            }
            ErrorCode::PermissionDenied => {
                "the official client login is restricted by configuration or administrator policy"
            }
            ErrorCode::ConfigConflict | ErrorCode::RevisionConflict => {
                "official authentication changed during login; try again"
            }
            _ => {
                "cannot complete or clean up the isolated official login; check the official client and try again"
            }
        },
    )
}

fn config_file(client: ClientKind) -> &'static str {
    match client {
        ClientKind::Codex => "config.toml",
        ClientKind::Claude => "settings.json",
    }
}

fn prepare_login_home(home: &Path, client: ClientKind, login_id: &str) -> Result<()> {
    fs::create_dir_all(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
        if let Some(parent) = home.parent() {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    write_private(
        &home.join(MARKER),
        &serde_json::to_vec(&LoginMarker {
            client,
            login_id: login_id.into(),
            child_pid: None,
            child_start: None,
            isolated_process_group: false,
            tree_terminated: false,
            spawn_pending: false,
        })?,
    )
}

fn clean_home(
    client: ClientKind,
    home: &Path,
    credentials: &dyn NativeCredentialStore,
) -> Result<()> {
    if !home.exists() {
        return Ok(());
    }
    cleanup_login_home_with_credentials(client, home, credentials)?;
    fs::remove_dir_all(home)?;
    Ok(())
}

async fn terminate_login_child(
    child: &mut tokio::process::Child,
    pid: u32,
    home: &Path,
    cleanup_allowed: &AtomicBool,
) -> std::result::Result<(), AppError> {
    process_identity::stop_live_child(child, pid).await.map_err(|_| failure(ErrorCode::ConfigUnavailable, "cannot verify that the isolated login process tree has exited; its temporary directory was retained"))?;
    cleanup_allowed.store(true, Ordering::Release);
    let recorded = fs::read(home.join(MARKER)).and_then(|bytes| {
        serde_json::from_slice::<LoginMarker>(&bytes).map_err(std::io::Error::other)
    });
    if let Ok(mut marker) = recorded {
        marker.tree_terminated = true;
        marker.spawn_pending = false;
        let _ = write_marker(home, &marker);
    }
    Ok(())
}

fn record_child_marker(
    session: &LoginSession,
    home: &Path,
    pid: Option<u32>,
) -> std::result::Result<(), AppError> {
    let status = session.status.read();
    let mut marker = LoginMarker {
        client: status.client,
        login_id: status.login_id.clone(),
        child_pid: pid,
        child_start: None,
        isolated_process_group: cfg!(unix),
        tree_terminated: false,
        spawn_pending: true,
    };
    write_marker(home, &marker).map_err(|error| login_error(&error))?;
    if let Some(pid) = pid {
        marker.child_start =
            Some(process_identity::process_start(pid).map_err(|error| login_error(&error))?);
        marker.spawn_pending = false;
        write_marker(home, &marker).map_err(|error| login_error(&error))?;
    }
    Ok(())
}

fn write_marker(home: &Path, marker: &LoginMarker) -> Result<()> {
    use std::io::Write;
    let mut file = atomic_write_file::AtomicWriteFile::open(home.join(MARKER))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&serde_json::to_vec(marker)?)?;
    file.commit()?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn find_client_executable(client: ClientKind) -> Result<PathBuf> {
    let name = client.as_str();
    let names = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.into()]
    };
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(base) = directories::BaseDirs::new() {
        directories.push(base.home_dir().join(".local/bin"));
    }
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    directories
        .into_iter()
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            DaemonError::Config(format!(
                "the official {} client is not installed or is not on PATH",
                client.as_str()
            ))
        })
}

fn client_command(
    executable: &Path,
    home: &Path,
    environment: &BTreeMap<OsString, OsString>,
) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(home)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in [
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
        "CLAUDE_CODE_OAUTH_SCOPES",
        "CLAUDE_CODE_OAUTH_CLIENT_ID",
        "CLAUDE_CODE_CUSTOM_OAUTH_URL",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "CLAUDECODE",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "ANTHROPIC_PROFILE",
        "ANTHROPIC_CONFIG_DIR",
        "ANTHROPIC_FEDERATION_RULE_ID",
        "ANTHROPIC_ORGANIZATION_ID",
        "ANTHROPIC_AUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_ACCOUNT_UUID",
        "CLAUDE_CODE_USER_EMAIL",
        "CLAUDE_CODE_ORGANIZATION_UUID",
    ] {
        command.env_remove(key);
    }
    for (key, value) in environment {
        if value.is_empty() {
            command.env_remove(key);
        } else {
            command.env(key, value);
        }
    }
    command
        .env("CODEX_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    #[cfg(unix)]
    command.process_group(0);
    command
}

fn login_command(
    client: ClientKind,
    executable: &Path,
    home: &Path,
    environment: &BTreeMap<OsString, OsString>,
) -> Command {
    let mut command = client_command(executable, home, environment);
    match client {
        ClientKind::Codex => {
            command.args([
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "cli_auth_credentials_store=\"file\"",
            ]);
        }
        ClientKind::Claude => {
            command.args(["auth", "login", "--claudeai"]);
        }
    }
    command
}
