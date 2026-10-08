use std::{collections::BTreeMap, ffi::OsString, path::Path, time::Duration};

use hsin_core::{AppError, ClientKind, ErrorCode};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, watch},
};
use zeroize::Zeroizing;

use super::{LoginSession, client_command, failure, record_child_marker, terminate_login_child};

const MAX_FRAME: u64 = 1024 * 1024;
const CLAUDE_MANUAL_CODE_VERSION: (u64, u64, u64) = (2, 1, 126);
const WINDOWS_CODEX_SECRETS_VERSION: (u64, u64, u64) = (0, 161, 0);

#[cfg(all(windows, not(test)))]
mod windows_compatibility;

// Keep the same fallible interface where production Windows performs a probe.
#[cfg_attr(any(not(windows), test), allow(clippy::unnecessary_wraps))]
pub(super) fn ensure_native_account_compatibility(client: ClientKind) -> crate::error::Result<()> {
    // Tests use the injected native credential store and never probe a real CLI.
    #[cfg(all(windows, not(test)))]
    if client == ClientKind::Codex {
        return windows_compatibility::ensure_codex_version();
    }
    let _ = client;
    Ok(())
}

pub(super) async fn check_client_capability(
    client: ClientKind,
    executable: &Path,
    home: &Path,
    environment: &BTreeMap<OsString, OsString>,
    session: &LoginSession,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let mut command = client_command(executable, home, environment);
    match client {
        ClientKind::Codex => {
            command.args(["app-server", "--help"]);
        }
        ClientKind::Claude => {
            command.args(["auth", "login", "--help"]);
        }
    }
    command.stdin(std::process::Stdio::null());
    let (successful, help) = capability_output(command, home, session, cancellation).await?;
    let mut compatible = successful
        && match client {
            ClientKind::Codex => help.contains("--listen"),
            ClientKind::Claude => help.contains("--claudeai"),
        };
    if compatible && (client == ClientKind::Claude || cfg!(windows)) {
        let mut version = client_command(executable, home, environment);
        version.arg("--version").stdin(std::process::Stdio::null());
        let (successful, version) = capability_output(version, home, session, cancellation).await?;
        compatible = successful
            && match client {
                ClientKind::Codex => {
                    meets_version_floor(client, &version, WINDOWS_CODEX_SECRETS_VERSION)
                }
                ClientKind::Claude => claude_supports_manual_code(&version),
            };
    }
    if compatible {
        Ok(())
    } else {
        Err(failure(
            ErrorCode::ProtocolMismatch,
            if cfg!(windows) && client == ClientKind::Codex {
                "Windows official accounts require Codex CLI 0.161.0 or newer; update Codex first"
            } else {
                "the installed official client does not support this login flow; update it first"
            },
        ))
    }
}

async fn capability_output(
    mut command: tokio::process::Command,
    home: &Path,
    session: &LoginSession,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<(bool, Zeroizing<String>), AppError> {
    if *cancellation.borrow() {
        return Err(failure(
            ErrorCode::Timeout,
            "official client capability check was cancelled",
        ));
    }
    record_child_marker(session, home, None)?;
    let mut child = command.spawn().map_err(|_| {
        failure(
            ErrorCode::ConfigUnavailable,
            "cannot launch the installed official client",
        )
    })?;
    session
        .cleanup_allowed
        .store(false, std::sync::atomic::Ordering::Release);
    let pid = child.id().ok_or_else(|| {
        failure(
            ErrorCode::Internal,
            "cannot identify the official capability process",
        )
    })?;
    if record_child_marker(session, home, Some(pid)).is_err() {
        // A --help/--version process can finish before process metadata is read.
        // Its held child handle still binds cleanup to the launched process.
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = terminate_login_child(&mut child, pid, home, &session.cleanup_allowed).await;
            return Err(failure(
                ErrorCode::ConfigUnavailable,
                "cannot record the isolated capability process",
            ));
        }
    }
    let stdout = child.stdout.take().ok_or_else(|| {
        failure(
            ErrorCode::Internal,
            "official capability output is unavailable",
        )
    })?;
    let progress = async {
        let mut bytes = Zeroizing::new(Vec::new());
        stdout
            .take(MAX_FRAME + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| {
                failure(
                    ErrorCode::ConfigUnavailable,
                    "cannot read official client capabilities",
                )
            })?;
        if bytes.len() as u64 > MAX_FRAME {
            return Err(failure(
                ErrorCode::FrameTooLarge,
                "official capability output exceeds the size limit",
            ));
        }
        let status = child.wait().await.map_err(|_| {
            failure(
                ErrorCode::ConfigUnavailable,
                "cannot wait for official capability process",
            )
        })?;
        Ok((
            status.success(),
            Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned()),
        ))
    };
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(10), progress) => result.map_err(|_| failure(ErrorCode::Timeout, "official client capability check timed out")).and_then(|result| result),
        _ = cancellation.changed() => Err(failure(ErrorCode::Timeout, "official client capability check was cancelled")),
    };
    terminate_login_child(&mut child, pid, home, &session.cleanup_allowed).await?;
    result
}

pub(super) fn claude_supports_manual_code(version: &str) -> bool {
    // Claude Code added pasted authorization codes to `auth login` in 2.1.126.
    meets_version_floor(ClientKind::Claude, version, CLAUDE_MANUAL_CODE_VERSION)
}

pub(super) fn meets_version_floor(
    client: ClientKind,
    output: &str,
    floor: (u64, u64, u64),
) -> bool {
    let mut words = output.split_whitespace();
    if client == ClientKind::Codex && words.next() != Some("codex-cli") {
        return false;
    }
    let Some(version) = words.next() else {
        return false;
    };
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some() || patch.contains(['-', '+']) {
        return false;
    }
    match (
        major.parse::<u64>(),
        minor.parse::<u64>(),
        patch.parse::<u64>(),
    ) {
        (Ok(major), Ok(minor), Ok(patch)) => (major, minor, patch) >= floor,
        _ => false,
    }
}

pub(super) async fn login(
    client: ClientKind,
    stdin: ChildStdin,
    stdout: ChildStdout,
    session: &LoginSession,
    incoming: mpsc::Receiver<Zeroizing<String>>,
) -> Result<(), AppError> {
    match client {
        ClientKind::Codex => codex_login(stdin, BufReader::new(stdout), session).await,
        ClientKind::Claude => claude_login(stdin, BufReader::new(stdout), session, incoming).await,
    }
}

async fn send_json(writer: &mut (impl AsyncWrite + Unpin), value: Value) -> Result<(), AppError> {
    let mut bytes = Zeroizing::new(serde_json::to_vec(&value).map_err(|_| {
        failure(
            ErrorCode::Internal,
            "cannot encode an official login request",
        )
    })?);
    bytes.push(b'\n');
    writer.write_all(&bytes).await.map_err(|_| {
        failure(
            ErrorCode::ConfigUnavailable,
            "official client login connection closed",
        )
    })?;
    writer.flush().await.map_err(|_| {
        failure(
            ErrorCode::ConfigUnavailable,
            "official client login connection closed",
        )
    })
}

async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Zeroizing<Vec<u8>>>, AppError> {
    let mut bytes = Zeroizing::new(Vec::new());
    let count = reader
        .take(MAX_FRAME + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| {
            failure(
                ErrorCode::ConfigUnavailable,
                "cannot read official client login progress",
            )
        })?;
    if count as u64 > MAX_FRAME {
        return Err(failure(
            ErrorCode::FrameTooLarge,
            "official client login response exceeds the size limit",
        ));
    }
    Ok((count != 0).then_some(bytes))
}

async fn read_json(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value, AppError> {
    loop {
        let line = read_line(reader).await?.ok_or_else(|| {
            failure(
                ErrorCode::AuthenticationFailed,
                "official client closed before login completed",
            )
        })?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        return serde_json::from_slice(&line).map_err(|_| {
            failure(
                ErrorCode::ProtocolMismatch,
                "official client returned an invalid login response",
            )
        });
    }
}

async fn initialize_codex(
    writer: &mut (impl AsyncWrite + Unpin),
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<(), AppError> {
    send_json(
        writer,
        json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"hsin","title":"hsin","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }}),
    )
    .await?;
    loop {
        let frame = read_json(reader).await?;
        if frame.get("id").and_then(Value::as_u64) == Some(1) {
            if frame.get("error").is_some() {
                return Err(failure(
                    ErrorCode::ProtocolMismatch,
                    "Codex app-server initialization failed; update Codex and try again",
                ));
            }
            break;
        }
    }
    send_json(writer, json!({"method":"initialized","params":{}})).await
}

pub(super) async fn codex_login(
    mut writer: impl AsyncWrite + Unpin,
    mut reader: impl AsyncBufRead + Unpin,
    session: &LoginSession,
) -> Result<(), AppError> {
    initialize_codex(&mut writer, &mut reader).await?;
    send_json(
        &mut writer,
        json!({"id":2,"method":"account/login/start","params":{"type":"chatgpt"}}),
    )
    .await?;
    let mut login_id = None;
    let mut completed = false;
    loop {
        let frame = read_json(&mut reader).await?;
        if frame.get("id").and_then(Value::as_u64) == Some(2) {
            if frame.get("error").is_some() {
                return Err(failure(
                    ErrorCode::AuthenticationFailed,
                    "Codex could not start the official login; check login restrictions and try again",
                ));
            }
            let result = frame.get("result").ok_or_else(|| {
                failure(
                    ErrorCode::ProtocolMismatch,
                    "Codex returned no login result",
                )
            })?;
            if result.get("type").and_then(Value::as_str) != Some("chatgpt") {
                return Err(failure(
                    ErrorCode::ProtocolMismatch,
                    "Codex returned an unsupported login method",
                ));
            }
            login_id = result
                .get("loginId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let url = result
                .get("authUrl")
                .and_then(Value::as_str)
                .filter(|url| valid_browser_url(ClientKind::Codex, url))
                .ok_or_else(|| {
                    failure(
                        ErrorCode::ProtocolMismatch,
                        "Codex returned no valid official login URL",
                    )
                })?;
            if login_id.is_none() {
                return Err(failure(
                    ErrorCode::ProtocolMismatch,
                    "Codex returned no login identifier",
                ));
            }
            session.awaiting_browser(url.into());
        } else if frame.get("method").and_then(Value::as_str) == Some("account/login/completed") {
            let params = frame.get("params").unwrap_or(&Value::Null);
            if params.get("loginId").and_then(Value::as_str) != login_id.as_deref() {
                continue;
            }
            if params.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(failure(
                    ErrorCode::AuthenticationFailed,
                    "Codex official login was not completed; try again",
                ));
            }
            completed = true;
            send_json(
                &mut writer,
                json!({"id":3,"method":"account/read","params":{"refreshToken":false}}),
            )
            .await?;
        } else if completed && frame.get("id").and_then(Value::as_u64) == Some(3) {
            let account = frame.pointer("/result/account");
            if frame.get("error").is_some()
                || account
                    .and_then(|account| account.get("type"))
                    .and_then(Value::as_str)
                    != Some("chatgpt")
            {
                return Err(failure(
                    ErrorCode::AuthenticationFailed,
                    "Codex login did not produce a ChatGPT account",
                ));
            }
            return Ok(());
        } else if frame.get("method").is_some()
            && let Some(id) = frame.get("id")
        {
            // Login never delegates credential refresh or arbitrary client requests.
            send_json(
                &mut writer,
                json!({"id":id,"error":{"code":-32601,"message":"unsupported login request"}}),
            )
            .await?;
        }
    }
}

pub(super) async fn claude_login(
    mut writer: impl AsyncWrite + Unpin,
    mut reader: impl AsyncBufRead + Unpin,
    session: &LoginSession,
    mut incoming: mpsc::Receiver<Zeroizing<String>>,
) -> Result<(), AppError> {
    let progress = async {
        let mut successful = false;
        while let Some(line) = read_line(&mut reader).await? {
            let line = Zeroizing::new(String::from_utf8_lossy(&line).into_owned());
            if let Some(url) = extract_claude_url(&line) {
                session.awaiting_browser(url);
            }
            if line.trim_end().ends_with("Login successful.") {
                successful = true;
            }
        }
        if successful {
            Ok(())
        } else {
            Err(failure(
                ErrorCode::AuthenticationFailed,
                "Claude Code official login was not completed; check login restrictions and try again",
            ))
        }
    };
    tokio::pin!(progress);
    let mut input_open = true;
    loop {
        tokio::select! {
            result = &mut progress => return result,
            code = incoming.recv(), if input_open => {
                let Some(code) = code else { input_open = false; continue; };
                writer.write_all(code.as_bytes()).await.map_err(|_| failure(ErrorCode::AuthenticationFailed, "Claude Code login no longer accepts authorization codes"))?;
                writer.write_all(b"\n").await.map_err(|_| failure(ErrorCode::AuthenticationFailed, "Claude Code login input closed"))?;
                writer.flush().await.map_err(|_| failure(ErrorCode::AuthenticationFailed, "Claude Code login input closed"))?;
            }
        }
    }
}

pub(super) fn valid_manual_code(code: &str) -> bool {
    if code.len() > 16 * 1024 || code.chars().any(char::is_control) {
        return false;
    }
    code.split_once('#').is_some_and(|(authorization, state)| {
        !authorization.is_empty() && !state.is_empty() && !state.contains('#')
    })
}

fn valid_browser_url(client: ClientKind, raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    matches!(
        (client, url.host_str()),
        (
            ClientKind::Codex,
            Some("auth.openai.com" | "auth0.openai.com" | "login.openai.com")
        ) | (
            ClientKind::Claude,
            Some("claude.ai" | "claude.com" | "platform.claude.com")
        )
    )
}

fn extract_claude_url(line: &str) -> Option<String> {
    line.match_indices("https://").find_map(|(offset, _)| {
        let raw = &line[offset..];
        let end = raw
            .find(|character: char| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '"' | '<' | '>')
            })
            .unwrap_or(raw.len());
        let raw = &raw[..end];
        valid_browser_url(ClientKind::Claude, raw).then(|| raw.into())
    })
}
