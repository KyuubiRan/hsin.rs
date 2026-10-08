//! Recover only a child whose recorded creation identity still matches.
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(not(windows))]
use std::process::Stdio;
use std::{path::Path, process::Command, thread, time::Duration};

use super::LoginMarker;
use crate::error::{DaemonError, Result};

#[cfg(windows)]
mod sync_output;
#[cfg(windows)]
mod windows_tree;

// Cold PowerShell startup can exceed five seconds while the system is busy.
// Metadata probes remain bounded and never authorize cleanup on a timeout.
#[cfg(windows)]
const WINDOWS_METADATA_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn process_start(pid: u32) -> Result<String> {
    if pid == 0 {
        return Err(unavailable());
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat.rsplit_once(')').ok_or_else(unavailable)?.1;
        fields
            .split_whitespace()
            .nth(19)
            .map(str::to_owned)
            .ok_or_else(unavailable)
    }
    #[cfg(target_os = "macos")]
    {
        read_command(Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "lstart="]))
    }
    #[cfg(windows)]
    {
        let expression = format!(
            "$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if($null -eq $p){{'[hsin:absent]'}}else{{$p.StartTime.ToFileTimeUtc()}}"
        );
        read_command(Command::new("powershell.exe").args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &expression,
        ]))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(unavailable())
    }
}

#[cfg(not(target_os = "linux"))]
fn read_command(command: &mut Command) -> Result<String> {
    #[cfg(windows)]
    let output = sync_output::bounded_output(command, WINDOWS_METADATA_TIMEOUT)?;
    #[cfg(not(windows))]
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    let value = String::from_utf8(output.stdout).map_err(|_| {
        DaemonError::Config("isolated process metadata query returned invalid UTF-8".into())
    })?;
    if value.trim() == "[hsin:absent]"
        || (cfg!(target_os = "macos") && output.status.code() == Some(1) && value.trim().is_empty())
    {
        return Err(DaemonError::NotFound(
            "isolated official login process".into(),
        ));
    }
    if !output.status.success() {
        return Err(DaemonError::Config(
            "isolated process metadata query failed".into(),
        ));
    }
    if value.trim().is_empty() {
        return Err(DaemonError::Config(
            "isolated process metadata query returned no identity".into(),
        ));
    }
    Ok(value.trim().into())
}

#[cfg(all(windows, not(test)))]
pub(super) fn stop_sync_child(child: &mut std::process::Child, pid: u32) -> Result<()> {
    let descendants = windows_tree::capture(pid);
    let terminated = windows_tree::terminate(pid, descendants.as_deref().unwrap_or_default());
    if !matches!(terminated, Ok(status) if status.success()) {
        let _ = child.kill();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(unavailable());
        }
        thread::sleep(Duration::from_millis(25));
    }
    terminated?;
    let descendants = descendants?;
    if windows_tree::all_gone(&descendants)? && windows_tree::capture(pid)?.is_empty() {
        Ok(())
    } else {
        Err(unavailable())
    }
}

pub(super) fn stop_marked_child(marker: &LoginMarker, home: &Path) -> Result<()> {
    if marker.tree_terminated {
        return Ok(());
    }
    let (Some(pid), Some(expected)) = (marker.child_pid, marker.child_start.as_deref()) else {
        return if marker.child_pid.is_some() || marker.spawn_pending {
            Err(unavailable())
        } else {
            Ok(())
        };
    };
    #[cfg(unix)]
    if marker.isolated_process_group && live_group_members(pid)?.is_empty() {
        return Ok(());
    }
    let actual = match process_start(pid) {
        Ok(actual) => actual,
        Err(error) if is_absent(&error) => {
            #[cfg(unix)]
            if !marker.isolated_process_group {
                return Ok(());
            }
            return Err(unavailable());
        }
        Err(error) => return Err(error),
    };
    if actual != expected {
        #[cfg(unix)]
        if marker.isolated_process_group {
            return Err(unavailable());
        }
        return Ok(());
    }
    // On Unix also bind the pid to this unique staging working directory. macOS
    // lstart has second precision, so this closes the same-second pid reuse gap.
    #[cfg(target_os = "linux")]
    if fs::read_link(format!("/proc/{pid}/cwd")).ok().as_deref() != Some(home) {
        return Err(unavailable());
    }
    #[cfg(target_os = "macos")]
    {
        let cwd = read_command(Command::new("/usr/sbin/lsof").args([
            "-a",
            "-p",
            &pid.to_string(),
            "-d",
            "cwd",
            "-Fn",
        ]))?;
        if !cwd.lines().any(|line| {
            line.strip_prefix('n')
                .is_some_and(|path| Path::new(path) == home)
        }) {
            return Err(unavailable());
        }
    }
    #[cfg(windows)]
    let _ = home;
    #[cfg(unix)]
    let status = signal_kill(pid, marker.isolated_process_group)?;
    #[cfg(windows)]
    let descendants = windows_tree::capture(pid)?;
    #[cfg(windows)]
    let status = windows_tree::terminate(pid, &descendants)?;
    #[cfg(not(any(unix, windows)))]
    return Err(unavailable());
    #[cfg(any(unix, windows))]
    {
        if !status.success() && process_start(pid).is_ok() {
            return Err(unavailable());
        }
        for _ in 0..30 {
            #[cfg(unix)]
            if marker.isolated_process_group {
                if live_group_members(pid)?.is_empty() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            #[cfg(windows)]
            if !windows_tree::all_gone(&descendants)? {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            match process_start(pid) {
                Ok(actual) if actual != expected => return Ok(()),
                Err(error) if is_absent(&error) => return Ok(()),
                _ => {}
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(unavailable())
    }
}

/// The unreaped child handle binds this pid to the process we launched.
pub(super) async fn stop_live_child(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        if !live_group_members(pid)?.is_empty() {
            let _ = signal_kill(pid, true)?;
        }
    }
    #[cfg(windows)]
    let descendants = windows_tree::capture(pid);
    #[cfg(windows)]
    let tree_result = {
        let tree_result = windows_tree::terminate(pid, descendants.as_deref().unwrap_or_default());
        if !matches!(tree_result, Ok(status) if status.success()) {
            let _ = child.start_kill();
        }
        tree_result
    };
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| unavailable())??;
    #[cfg(windows)]
    let descendants = {
        tree_result?;
        descendants?
    };
    for _ in 0..30 {
        #[cfg(unix)]
        if live_group_members(pid)?.is_empty() {
            return Ok(());
        }
        #[cfg(windows)]
        if windows_tree::all_gone(&descendants)? && windows_tree::capture(pid)?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(unavailable())
}

#[cfg(unix)]
fn signal_kill(pid: u32, group: bool) -> Result<std::process::ExitStatus> {
    let target = if group {
        format!("-{pid}")
    } else {
        pid.to_string()
    };
    Ok(Command::new("/bin/kill")
        // Linux kill otherwise parses a negative process-group id as another
        // signal option, so separate the target from the options explicitly.
        .args(["-KILL", "--", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?)
}

#[cfg(unix)]
fn live_group_members(group: u32) -> Result<Vec<u32>> {
    #[cfg(target_os = "linux")]
    {
        let mut members = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            let stat = match fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let fields = stat
                .rsplit_once(')')
                .ok_or_else(unavailable)?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            if fields.get(2).and_then(|value| value.parse::<u32>().ok()) == Some(group)
                && fields.first() != Some(&"Z")
            {
                members.push(pid);
            }
        }
        Ok(members)
    }
    #[cfg(target_os = "macos")]
    {
        let listing = read_command(Command::new("/bin/ps").args(["-axo", "pid=,pgid=,stat="]))?;
        let mut members = Vec::new();
        for line in listing.lines() {
            let mut fields = line.split_whitespace();
            let pid = fields
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(unavailable)?;
            let actual_group = fields
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(unavailable)?;
            let state = fields.next().ok_or_else(unavailable)?;
            if actual_group == group && !state.starts_with('Z') {
                members.push(pid);
            }
        }
        Ok(members)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = group;
        Err(unavailable())
    }
}

pub(super) fn is_absent(error: &DaemonError) -> bool {
    if matches!(error, DaemonError::NotFound(_)) {
        return true;
    }
    #[cfg(target_os = "linux")]
    if matches!(error, DaemonError::Io(error) if error.kind() == std::io::ErrorKind::NotFound) {
        return true;
    }
    false
}

fn unavailable() -> DaemonError {
    DaemonError::Config("cannot verify or stop the isolated official login process".into())
}
