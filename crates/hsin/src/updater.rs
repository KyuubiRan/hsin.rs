#[cfg(unix)]
use std::io;
use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[cfg(unix)]
const INSTALLER_URL: &str =
    "https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.sh";
#[cfg(windows)]
const INSTALLER_URL: &str =
    "https://raw.githubusercontent.com/KyuubiRan/hsin.rs/main/scripts/install.ps1";

pub async fn run() -> Result<()> {
    let executable = env::current_exe()
        .context("cannot locate the hsin executable")?
        .canonicalize()
        .context("cannot resolve the hsin executable")?;
    let script = temporary_script_path();
    let installer_url = env::var("HSIN_INSTALLER_URL").unwrap_or_else(|_| INSTALLER_URL.into());
    let result = async {
        download_installer(&script, &installer_url).await?;
        execute_installer(&script, &executable).await
    }
    .await;
    let _ = std::fs::remove_file(&script);
    result
}

fn temporary_script_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let extension = if cfg!(windows) { "ps1" } else { "sh" };
    env::temp_dir().join(format!(
        "hsin-update-{}-{nonce}.{extension}",
        std::process::id()
    ))
}

#[cfg(unix)]
async fn download_installer(script: &Path, installer_url: &str) -> Result<()> {
    match download_with(
        "curl",
        &["-fsSL", installer_url, "-o"],
        script,
        installer_url,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            download_with("wget", &["-q", "-O"], script, installer_url)
                .await
                .context("download the hsin installer with wget")
        }
        Err(error) => Err(error).context("download the hsin installer with curl"),
    }
}

#[cfg(unix)]
async fn download_with(
    program: &str,
    args: &[&str],
    script: &Path,
    installer_url: &str,
) -> io::Result<()> {
    let mut command = Command::new(program);
    if program == "curl" {
        command.args(args).arg(script);
    } else {
        command.args(args).arg(script).arg(installer_url);
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}

#[cfg(windows)]
async fn download_installer(script: &Path, installer_url: &str) -> Result<()> {
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Invoke-WebRequest -UseBasicParsing -Uri $env:HSIN_INSTALLER_URL -OutFile $env:HSIN_INSTALLER_PATH",
        ])
        .env("HSIN_INSTALLER_URL", installer_url)
        .env("HSIN_INSTALLER_PATH", script)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("start PowerShell to download the hsin installer")?;
    if !status.success() {
        bail!("PowerShell installer download exited with {status}");
    }
    Ok(())
}

async fn execute_installer(script: &Path, executable: &Path) -> Result<()> {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command
    } else {
        Command::new("sh")
    };
    command.arg(script);
    command
        .env("HSIN_CURRENT_VERSION", env!("CARGO_PKG_VERSION"))
        .env("HSIN_EXECUTABLE", executable)
        .env_remove("HSIN_VERSION")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if env::var_os("HSIN_INSTALL_DIR").is_none() {
        let directory = executable
            .parent()
            .context("hsin executable has no parent directory")?;
        command.env("HSIN_INSTALL_DIR", directory);
    }
    #[cfg(windows)]
    command.env("HSIN_UPDATE_PARENT_PID", std::process::id().to_string());

    let status = command
        .status()
        .await
        .context("execute the hsin installer")?;
    if !status.success() {
        bail!("hsin installer exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_scripts_are_platform_specific_and_unique() {
        let first = temporary_script_path();
        let second = temporary_script_path();
        assert_eq!(
            first.extension().and_then(|value| value.to_str()),
            Some(if cfg!(windows) { "ps1" } else { "sh" })
        );
        assert_ne!(first, second);
    }
}
