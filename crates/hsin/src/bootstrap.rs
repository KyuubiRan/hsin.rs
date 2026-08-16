use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
#[cfg(any(windows, test))]
use std::collections::BTreeMap;

use hsin_core::DoctorFinding;
#[cfg(any(windows, test))]
use hsin_core::DoctorSeverity;
use tokio::process::Command;

#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskInspection {
    Present,
    Missing,
    Failed(String),
}

pub async fn install_and_start() -> Result<()> {
    run_service(&["install", "--start"]).await
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
        #[cfg(windows)]
        if let Some(hint) = missing_installed_daemon_hint() {
            bail!("hsind service command exited with {status}; {hint}");
        }
        bail!("hsind service command exited with {status}");
    }
    Ok(())
}

fn sibling_daemon() -> Result<PathBuf> {
    let candidate = sibling_daemon_candidate()?;
    if candidate.is_file() {
        return Ok(candidate);
    }

    // Cargo places sibling workspace binaries in the same target directory. During
    // development this gives a useful error without silently searching PATH for a
    // potentially unrelated daemon.
    #[cfg(windows)]
    bail!(
        "sibling daemon is missing at {}; Windows Security or another security product may have quarantined hsind.exe; check Protection History before retrying",
        candidate.display()
    );
    #[cfg(not(windows))]
    bail!(
        "sibling daemon not found at {}; set HSIND_PATH to override",
        candidate.display()
    )
}

fn sibling_daemon_candidate() -> Result<PathBuf> {
    if let Some(path) = env::var_os("HSIND_PATH") {
        return Ok(PathBuf::from(path));
    }

    let executable = env::current_exe().context("cannot locate the hsin executable")?;
    let directory = executable
        .parent()
        .context("hsin executable has no parent directory")?;
    Ok(directory.join(daemon_file_name()))
}

pub fn doctor_findings() -> Vec<DoctorFinding> {
    #[cfg(windows)]
    {
        let home = hsin_ipc::data_home();
        let installed_daemon = home.join("bin").join(daemon_file_name());
        let source_daemon = sibling_daemon_candidate().ok();
        let task_label = format!("dev.hsin.hsind.{}", hsin_ipc::home_scope(&home));
        let task = match std::process::Command::new("schtasks")
            .args(["/Query", "/TN", &task_label])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => TaskInspection::Present,
            Ok(_) => TaskInspection::Missing,
            Err(error) => TaskInspection::Failed(error.to_string()),
        };
        return windows_installation_findings(&WindowsInstallationState {
            marker_exists: home.join(hsin_ipc::INSTALL_HOME_MARKER).is_file(),
            installed_daemon_exists: installed_daemon.is_file(),
            installed_daemon,
            source_daemon_exists: source_daemon.as_ref().is_some_and(|path| path.is_file()),
            source_daemon,
            task_label,
            task,
        });
    }

    #[cfg(not(windows))]
    Vec::new()
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct WindowsInstallationState {
    marker_exists: bool,
    installed_daemon: PathBuf,
    installed_daemon_exists: bool,
    source_daemon: Option<PathBuf>,
    source_daemon_exists: bool,
    task_label: String,
    task: TaskInspection,
}

#[cfg(any(windows, test))]
fn windows_installation_findings(state: &WindowsInstallationState) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();
    if state.marker_exists && !state.installed_daemon_exists {
        findings.push(doctor_finding(
            "daemon_binary_missing",
            DoctorSeverity::Error,
            [("path", state.installed_daemon.display().to_string())],
        ));
    }
    if let Some(source) = &state.source_daemon
        && !state.source_daemon_exists
        && source != &state.installed_daemon
    {
        findings.push(doctor_finding(
            "daemon_source_missing",
            DoctorSeverity::Error,
            [("path", source.display().to_string())],
        ));
    }
    match &state.task {
        TaskInspection::Present if !state.marker_exists => findings.push(doctor_finding(
            "service_definition_orphaned",
            DoctorSeverity::Warning,
            [("task", state.task_label.clone())],
        )),
        TaskInspection::Missing if state.marker_exists => findings.push(doctor_finding(
            "service_definition_missing",
            DoctorSeverity::Warning,
            [("task", state.task_label.clone())],
        )),
        TaskInspection::Failed(message) => findings.push(doctor_finding(
            "service_check_failed",
            DoctorSeverity::Warning,
            [("message", message.clone())],
        )),
        TaskInspection::Present | TaskInspection::Missing => {}
    }
    findings
}

#[cfg(any(windows, test))]
fn doctor_finding<const N: usize>(
    code: &str,
    severity: DoctorSeverity,
    args: [(&str, String); N],
) -> DoctorFinding {
    DoctorFinding {
        code: code.into(),
        severity,
        args: args
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[cfg(windows)]
fn missing_installed_daemon_hint() -> Option<String> {
    let home = hsin_ipc::data_home();
    let daemon = home.join("bin").join(daemon_file_name());
    (home.join(hsin_ipc::INSTALL_HOME_MARKER).is_file() && !daemon.is_file()).then(|| {
        format!(
            "the installed daemon is missing at {}; Windows Security or another security product may have quarantined it; check Protection History before retrying",
            daemon.display()
        )
    })
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

    #[test]
    fn missing_installed_windows_daemon_is_reported_as_an_error() {
        let state = WindowsInstallationState {
            marker_exists: true,
            installed_daemon: PathBuf::from(r"C:\Users\kitsune\AppData\Local\hsin\bin\hsind.exe"),
            installed_daemon_exists: false,
            source_daemon: Some(PathBuf::from(
                r"C:\Users\kitsune\AppData\Local\Programs\hsin\hsind.exe",
            )),
            source_daemon_exists: true,
            task_label: "dev.hsin.hsind.example".into(),
            task: TaskInspection::Present,
        };

        let findings = windows_installation_findings(&state);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "daemon_binary_missing");
        assert_eq!(findings[0].severity, DoctorSeverity::Error);
        assert!(findings[0].args["path"].ends_with("hsind.exe"));
    }

    #[test]
    fn incomplete_and_orphaned_windows_service_definitions_are_distinct() {
        let installed = PathBuf::from(r"C:\hsin\bin\hsind.exe");
        let base = WindowsInstallationState {
            marker_exists: true,
            installed_daemon: installed.clone(),
            installed_daemon_exists: true,
            source_daemon: Some(installed),
            source_daemon_exists: true,
            task_label: "dev.hsin.hsind.example".into(),
            task: TaskInspection::Missing,
        };
        assert_eq!(
            windows_installation_findings(&base)[0].code,
            "service_definition_missing"
        );

        let orphaned = WindowsInstallationState {
            marker_exists: false,
            task: TaskInspection::Present,
            ..base
        };
        assert_eq!(
            windows_installation_findings(&orphaned)[0].code,
            "service_definition_orphaned"
        );
    }

    #[test]
    fn windows_task_scheduler_check_failures_remain_actionable() {
        let state = WindowsInstallationState {
            marker_exists: false,
            installed_daemon: PathBuf::from(r"C:\hsin\bin\hsind.exe"),
            installed_daemon_exists: false,
            source_daemon: None,
            source_daemon_exists: false,
            task_label: "dev.hsin.hsind.example".into(),
            task: TaskInspection::Failed("schtasks.exe was not found".into()),
        };

        let findings = windows_installation_findings(&state);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "service_check_failed");
        assert_eq!(findings[0].args["message"], "schtasks.exe was not found");
    }
}
