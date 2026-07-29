use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use std::fmt::Write as _;

use crate::{
    crypto::{KeyStore, KeyStoreKind, SystemKeyStore},
    error::{DaemonError, Result},
    paths::Paths,
};
use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;

pub fn install(start: bool) -> Result<()> {
    let was_running = status()?;
    // Stop stale service wrappers as well as a currently locked daemon before
    // replacing binaries. This matters on Windows between restart attempts.
    stop()?;
    let paths = Paths::discover();
    paths.prepare()?;
    fs::write(
        paths.home.join(hsin_ipc::INSTALL_HOME_MARKER),
        hsin_ipc::INSTALL_HOME_MARKER_CONTENT,
    )?;
    let bin = paths.home.join("bin");
    fs::create_dir_all(&bin)?;
    let current = std::env::current_exe()?;
    install_binary(&current, &bin.join(exe_name("hsind")))?;
    let cli = current.with_file_name(exe_name("hsin"));
    if cli.exists() {
        install_binary(&cli, &bin.join(exe_name("hsin")))?;
    }
    install_definition(&paths, &bin.join(exe_name("hsind")))?;
    if start || was_running {
        self::start()?;
    }
    Ok(())
}

pub fn uninstall(purge: bool) -> Result<()> {
    let paths = Paths::discover();
    stop()?;
    #[cfg(target_os = "macos")]
    uninstall_definition(&paths)?;
    #[cfg(any(target_os = "linux", windows))]
    uninstall_definition(&paths);
    let _ = fs::remove_file(paths.home.join("bin").join(exe_name("hsind")));
    let _ = fs::remove_file(paths.home.join("bin").join(exe_name("hsin")));
    let _ = fs::remove_file(paths.home.join("bin/run-hsind.cmd"));
    if purge {
        let store = SystemKeyStore::for_home(&paths.home);
        for version in stored_key_versions(&paths.database)? {
            store.delete(version)?;
        }
        if paths.home.exists() {
            fs::remove_dir_all(paths.home)?;
        }
    }
    Ok(())
}

fn stored_key_versions(database: &Path) -> Result<Vec<u32>> {
    if !database.exists() {
        return Ok(vec![1]);
    }
    let connection = rusqlite::Connection::open(database)?;
    let mut statement =
        connection.prepare("SELECT version FROM encryption_keys ORDER BY version")?;
    let versions = statement
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(versions)
}

pub fn start() -> Result<()> {
    let paths = Paths::discover();
    if instance_running(&paths)? {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let label = service_label(&paths);
        let domain = format!("gui/{}", uid()?);
        let plist = launch_agent_path(&paths)?;
        let bootstrapped = Command::new("launchctl")
            .args(["bootstrap", &domain, &plist.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?
            .success();
        if !bootstrapped {
            command(
                Command::new("launchctl")
                    .args(["kickstart", &format!("{domain}/{label}")])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
            )?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let unit = service_unit(&paths);
        if systemd_user_available() {
            command(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            command(Command::new("systemctl").args(["--user", "enable", "--now", &unit]))?;
        } else {
            spawn_fallback()?;
        }
    }
    #[cfg(windows)]
    {
        command(Command::new("schtasks").args(["/Run", "/TN", &service_label(&paths)]))?;
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let paths = Paths::discover();
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", uid()?);
        let plist = launch_agent_path(&paths)?;
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &plist.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let unit = service_unit(&paths);
        if systemd_user_available() {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &unit])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        stop_fallback()?;
    }
    #[cfg(windows)]
    {
        let label = service_label(&paths);
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", &label])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    wait_for_instance_stop(&paths)
}

pub fn restart() -> Result<()> {
    stop()?;
    start()
}

pub fn status() -> Result<bool> {
    let paths = Paths::discover();
    instance_running(&paths)
}

fn instance_running(paths: &Paths) -> Result<bool> {
    lock_file_held(&paths.lock)
}

fn lock_file_held(path: &Path) -> Result<bool> {
    let file = match File::options().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

fn wait_for_instance_stop(paths: &Paths) -> Result<()> {
    for _ in 0..50 {
        if !instance_running(paths)? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(DaemonError::Internal(
        "daemon did not stop before the service timeout".into(),
    ))
}

fn install_binary(source: &Path, destination: &Path) -> Result<()> {
    if source == destination {
        return Ok(());
    }
    let mut input = File::open(source)?;
    let mut output = AtomicWriteFile::open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.commit()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_definition(paths: &Paths, daemon: &Path) -> Result<()> {
    let label = service_label(paths);
    let plist = launch_agent_path(paths)?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut variables = String::new();
    for (key, value) in service_environment(paths)? {
        write!(
            &mut variables,
            "<key>{}</key><string>{}</string>",
            xml(key),
            xml(&value)
        )
        .expect("writing to a String cannot fail");
    }
    let environment = format!("<key>EnvironmentVariables</key><dict>{variables}</dict>");
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>{}</string><string>run</string></array>
{}
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
<key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml(&daemon.to_string_lossy()),
        environment,
        xml(&paths.logs.join("hsind.stdout.log").to_string_lossy()),
        xml(&paths.logs.join("hsind.stderr.log").to_string_lossy())
    );
    fs::write(plist, content)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_definition(paths: &Paths, daemon: &Path) -> Result<()> {
    if systemd_user_available() {
        let unit = service_unit(paths);
        let path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| DaemonError::Internal("HOME is not set".into()))?
            .join(".config/systemd/user")
            .join(&unit);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut environment = String::new();
        for (key, value) in service_environment(paths)? {
            let assignment = format!("{key}={value}");
            writeln!(
                &mut environment,
                "Environment={}",
                systemd_quote(&assignment)
            )
            .expect("writing to a String cannot fail");
        }
        fs::write(
            path,
            format!(
                "[Unit]\nDescription=hsin provider daemon\n\n[Service]\nExecStart={} run\nRestart=on-failure\n{}\n[Install]\nWantedBy=default.target\n",
                systemd_quote(&daemon.to_string_lossy()),
                environment
            ),
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn install_definition(paths: &Paths, daemon: &Path) -> Result<()> {
    let wrapper = paths.home.join("bin/run-hsind.cmd");
    let mut variables = String::new();
    for (key, value) in service_environment(paths)? {
        write!(
            &mut variables,
            "set \"{key}={}\"\r\n",
            value.replace('%', "%%")
        )
        .expect("writing to a String cannot fail");
    }
    let daemon = daemon.to_string_lossy().replace('%', "%%");
    fs::write(
        &wrapper,
        format!(
            "@echo off\r\n{variables}:run\r\n\"{daemon}\" run\r\nif not errorlevel 1 exit /b 0\r\n>nul 2>&1 timeout /t 2 /nobreak\r\ngoto run\r\n"
        ),
    )?;
    let task_command = format!("\"{}\"", wrapper.display());
    command(Command::new("schtasks").args([
        "/Create",
        "/F",
        "/SC",
        "ONLOGON",
        "/RL",
        "LIMITED",
        "/TN",
        &service_label(paths),
        "/TR",
        &task_command,
    ]))
}

#[cfg(target_os = "macos")]
fn uninstall_definition(paths: &Paths) -> Result<()> {
    let domain = format!("gui/{}", uid()?);
    let plist = launch_agent_path(paths)?;
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = fs::remove_file(plist);
    Ok(())
}
#[cfg(target_os = "linux")]
fn uninstall_definition(paths: &Paths) {
    let unit = service_unit(paths);
    if systemd_user_available() {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", &unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let _ = fs::remove_file(PathBuf::from(home).join(".config/systemd/user").join(unit));
    }
}
#[cfg(windows)]
fn uninstall_definition(paths: &Paths) {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", &service_label(paths)])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(target_os = "macos")]
fn launch_agent_path(paths: &Paths) -> Result<PathBuf> {
    Ok(std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| DaemonError::Internal("HOME is not set".into()))?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", service_label(paths))))
}
#[cfg(target_os = "macos")]
fn uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(DaemonError::Internal("id -u failed".into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
#[cfg(target_os = "macos")]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
#[cfg(target_os = "linux")]
fn spawn_fallback() -> Result<()> {
    let paths = Paths::discover();
    if fallback_status()? {
        return Ok(());
    }
    let daemon = paths.home.join("bin/hsind");
    let child = Command::new("sh")
        .args([
            "-c",
            "if command -v setsid >/dev/null 2>&1; then exec setsid \"$1\" run; fi; trap '' HUP; exec \"$1\" run",
            "hsind-fallback",
        ])
        .arg(daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    fs::write(fallback_pid_path(&paths), child.id().to_string())?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn stop_fallback() -> Result<()> {
    let paths = Paths::discover();
    let pid_path = fallback_pid_path(&paths);
    let Some(pid) = read_fallback_pid(&pid_path)? else {
        return Ok(());
    };
    if !fallback_process_matches(pid, &paths) {
        let _ = fs::remove_file(pid_path);
        return Ok(());
    }
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    for _ in 0..50 {
        if !fallback_process_matches(pid, &paths) {
            let _ = fs::remove_file(pid_path);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(DaemonError::Internal(
        "fallback daemon did not stop before the service timeout".into(),
    ))
}
#[cfg(target_os = "linux")]
fn fallback_status() -> Result<bool> {
    let paths = Paths::discover();
    let pid_path = fallback_pid_path(&paths);
    let Some(pid) = read_fallback_pid(&pid_path)? else {
        return Ok(false);
    };
    if fallback_process_matches(pid, &paths) {
        Ok(true)
    } else {
        let _ = fs::remove_file(pid_path);
        Ok(false)
    }
}
#[cfg(target_os = "linux")]
fn fallback_process_matches(pid: u32, paths: &Paths) -> bool {
    let expected = paths.home.join("bin/hsind");
    let expected = expected.canonicalize().unwrap_or(expected);
    fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|actual| actual == expected)
}
#[cfg(target_os = "linux")]
fn read_fallback_pid(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(value) => value
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| DaemonError::Internal("invalid fallback daemon PID".into())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
#[cfg(target_os = "linux")]
fn fallback_pid_path(paths: &Paths) -> PathBuf {
    paths.home.join("hsind.pid")
}
#[cfg(target_os = "linux")]
fn systemd_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
#[cfg(target_os = "linux")]
pub fn uses_fallback() -> bool {
    !systemd_user_available()
}
fn command(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            detail
        };
        Err(DaemonError::Internal(format!(
            "service command exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )))
    }
}
#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}
fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    }
}

fn service_environment(paths: &Paths) -> Result<Vec<(&'static str, String)>> {
    service_environment_with(paths, |key| std::env::var_os(key))
}

fn service_environment_with(
    paths: &Paths,
    environment: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<(&'static str, String)>> {
    let mut variables = vec![("HSIN_HOME", absolute_path(&paths.home)?)];
    for key in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Some(value) = environment(key) {
            variables.push((key, absolute_path(&PathBuf::from(value))?));
        }
    }
    if let Some(value) = environment(KeyStoreKind::ENV) {
        let value = value.into_string().map_err(|_| {
            DaemonError::Invalid(format!("{} contains invalid characters", KeyStoreKind::ENV))
        })?;
        let kind = value.parse::<KeyStoreKind>()?;
        variables.push((KeyStoreKind::ENV, kind.as_str().to_owned()));
    }
    Ok(variables)
}

fn absolute_path(path: &Path) -> Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(any(target_os = "macos", windows))]
fn service_label(paths: &Paths) -> String {
    format!("dev.hsin.hsind.{}", hsin_ipc::home_scope(&paths.home))
}

#[cfg(target_os = "linux")]
fn service_unit(paths: &Paths) -> String {
    format!("hsind-{}.service", hsin_ipc::home_scope(&paths.home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_environment_propagates_the_key_store_backend() {
        let directory =
            std::env::temp_dir().join(format!("hsin-service-environment-{}", uuid::Uuid::new_v4()));
        let paths = Paths {
            database: directory.join("hsin.sqlite3"),
            lock: directory.join("hsind.lock"),
            logs: directory.join("logs"),
            backups: directory.join("backups"),
            home: directory,
        };
        let variables = service_environment_with(&paths, |key| match key {
            "CODEX_HOME" => Some(std::ffi::OsString::from("/tmp/codex")),
            KeyStoreKind::ENV => Some(std::ffi::OsString::from(" FILE ")),
            _ => None,
        })
        .unwrap();

        assert!(variables.contains(&("HSIN_KEYSTORE", "file".to_owned())));
        assert!(variables.contains(&("CODEX_HOME", "/tmp/codex".to_owned())));
    }

    #[test]
    fn service_status_treats_any_state_owner_lock_as_running() {
        let directory = std::env::temp_dir().join(format!("hsin-service-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let lock_path = directory.join("hsind.lock");

        assert!(!lock_file_held(&lock_path).unwrap());
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        assert!(lock_file_held(&lock_path).unwrap());
        FileExt::unlock(&lock).unwrap();
        assert!(!lock_file_held(&lock_path).unwrap());

        fs::remove_dir_all(directory).unwrap();
    }
}
