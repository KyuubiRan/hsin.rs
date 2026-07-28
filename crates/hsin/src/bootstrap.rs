use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

pub async fn install_and_start() -> Result<()> {
    run_service(&["install", "--start"]).await
}

/// Whether a daemon binary is deployed next to this CLI (or via `HSIND_PATH`).
/// Single-binary installs without `hsind` run in standalone mode instead.
pub fn daemon_available() -> bool {
    sibling_daemon().is_ok()
}

pub async fn service_status() -> Result<bool> {
    let daemon = sibling_daemon()?;
    let output = Command::new(&daemon)
        .args(["service", "status"])
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {}", daemon.display()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => bail!(
            "hsind service status exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub async fn run_service(args: &[&str]) -> Result<()> {
    let daemon = sibling_daemon()?;
    let status = Command::new(&daemon)
        .arg("service")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| format!("failed to execute {}", daemon.display()))?;
    if !status.success() {
        bail!("hsind service command exited with {status}");
    }
    Ok(())
}

fn sibling_daemon() -> Result<PathBuf> {
    if let Some(path) = env::var_os("HSIND_PATH") {
        return Ok(PathBuf::from(path));
    }

    let executable = env::current_exe().context("cannot locate the hsin executable")?;
    let directory = executable
        .parent()
        .context("hsin executable has no parent directory")?;
    let candidate = directory.join(daemon_file_name());
    if candidate.is_file() {
        return Ok(candidate);
    }

    // Cargo places sibling workspace binaries in the same target directory. During
    // development this gives a useful error without silently searching PATH for a
    // potentially unrelated daemon.
    bail!(
        "sibling daemon not found at {}; set HSIND_PATH to override",
        candidate.display()
    )
}

fn daemon_file_name() -> &'static Path {
    if cfg!(windows) {
        Path::new("hsind.exe")
    } else {
        Path::new("hsind")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_name_matches_platform() {
        assert_eq!(daemon_file_name().extension().is_some(), cfg!(windows));
    }
}
