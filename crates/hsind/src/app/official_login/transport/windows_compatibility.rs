//! Bounded, isolated version inspection; this never starts OAuth or installs a CLI.
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    process::Stdio,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use hsin_core::ClientKind;
use zeroize::Zeroizing;

use super::super::{
    LoginMarker, client_command, find_client_executable, prepare_login_home, process_identity,
    write_marker,
};
use super::{WINDOWS_CODEX_SECRETS_VERSION, meets_version_floor};
use crate::error::{DaemonError, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VERSION_BYTES: u64 = 4096;

fn upgrade_required() -> DaemonError {
    DaemonError::Config("Windows official accounts require Codex CLI 0.161.0 or newer; install or update Codex before switching accounts".into())
}

pub(super) fn ensure_codex_version() -> Result<()> {
    let executable = find_client_executable(ClientKind::Codex).map_err(|_| upgrade_required())?;
    let login_id = uuid::Uuid::new_v4().to_string();
    let home = std::env::temp_dir().join(format!("hsin-codex-version-{login_id}"));
    prepare_login_home(&home, ClientKind::Codex, &login_id).map_err(|_| upgrade_required())?;
    let mut marker = LoginMarker {
        client: ClientKind::Codex,
        login_id,
        child_pid: None,
        child_start: None,
        isolated_process_group: false,
        tree_terminated: false,
        spawn_pending: true,
    };
    write_marker(&home, &marker).map_err(|_| upgrade_required())?;
    let mut command = client_command(&executable, &home, &BTreeMap::new());
    command.arg("--version").stdin(Stdio::null());
    let Ok(mut child) = command.as_std_mut().spawn() else {
        let _ = fs::remove_dir_all(&home);
        return Err(upgrade_required());
    };
    let pid = child.id();
    marker.child_pid = Some(pid);
    marker.child_start = process_identity::process_start(pid).ok();
    let _ = write_marker(&home, &marker);
    let Some(stdout) = child.stdout.take() else {
        if process_identity::stop_sync_child(&mut child, pid).is_ok() {
            let _ = fs::remove_dir_all(&home);
        }
        return Err(upgrade_required());
    };
    let (sender, received) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let result = stdout
            .take(MAX_VERSION_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => break None,
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = received
        .recv_timeout(remaining)
        .ok()
        .and_then(std::result::Result::ok);
    // Keep the directory if the launcher tree cannot be proven terminated.
    process_identity::stop_sync_child(&mut child, pid).map_err(|_| upgrade_required())?;
    marker.tree_terminated = true;
    marker.spawn_pending = false;
    let _ = write_marker(&home, &marker);
    let _ = fs::remove_dir_all(&home);
    let compatible = exited.is_some_and(|status| status.success())
        && output.is_some_and(|bytes| {
            bytes.len() as u64 <= MAX_VERSION_BYTES
                && meets_version_floor(
                    ClientKind::Codex,
                    &String::from_utf8_lossy(&bytes),
                    WINDOWS_CODEX_SECRETS_VERSION,
                )
        });
    if compatible {
        Ok(())
    } else {
        Err(upgrade_required())
    }
}
