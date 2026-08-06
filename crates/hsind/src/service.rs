use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use std::fmt::Write as _;

use crate::{
    crypto::{KeyStore, SystemKeyStore},
    error::{DaemonError, Result},
    paths::Paths,
};
use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;

/// Which service definition a command operates on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scope {
    /// Per-user definition owned by the calling account.
    #[default]
    User,
    /// systemd system unit that runs as a fixed account. Linux only.
    System,
}

/// A resolved service target. System scope runs as root while owning another
/// account's data home, so the paths cannot come from this process's
/// environment the way [`Paths::discover`] resolves them.
pub struct Target {
    paths: Paths,
    scope: Scope,
    #[cfg(target_os = "linux")]
    account: Option<Account>,
}

impl Target {
    pub fn resolve(scope: Scope, account: Option<&str>) -> Result<Self> {
        match scope {
            Scope::User => Ok(Self {
                paths: Paths::discover(),
                scope,
                #[cfg(target_os = "linux")]
                account: None,
            }),
            Scope::System => Self::system(account),
        }
    }

    #[cfg(target_os = "linux")]
    fn system(account: Option<&str>) -> Result<Self> {
        let name = account
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("SUDO_USER")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .ok_or_else(|| {
                DaemonError::Invalid(
                    "system scope needs the owning account; pass --account <name>".into(),
                )
            })?;
        let account = Account::lookup(&name)?;
        // An explicit HSIN_HOME still wins so portable and test instances keep
        // working, but the default must follow the owning account, not root.
        let home = hsin_ipc::hsin_home_override()
            .unwrap_or_else(|| hsin_ipc::data_home_for_account(&account.home));
        Ok(Self {
            paths: Paths::for_home(home),
            scope: Scope::System,
            account: Some(account),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn system(_account: Option<&str>) -> Result<Self> {
        Err(DaemonError::Invalid(
            "system scope is only supported on Linux".into(),
        ))
    }
}

/// A local account resolved from the passwd database.
#[cfg(target_os = "linux")]
struct Account {
    name: String,
    uid: String,
    gid: String,
    home: PathBuf,
}

#[cfg(target_os = "linux")]
impl Account {
    fn lookup(name: &str) -> Result<Self> {
        let output = Command::new("getent").args(["passwd", name]).output()?;
        if !output.status.success() {
            return Err(DaemonError::NotFound(format!("account {name}")));
        }
        Self::parse(&String::from_utf8_lossy(&output.stdout))
    }

    fn parse(entry: &str) -> Result<Self> {
        let fields: Vec<&str> = entry.trim_end().split(':').collect();
        // name:password:uid:gid:gecos:home:shell
        let [name, _, uid, gid, _, home, ..] = fields.as_slice() else {
            return Err(DaemonError::Internal(
                "unexpected passwd entry layout".into(),
            ));
        };
        if home.is_empty() {
            return Err(DaemonError::Invalid(format!(
                "account {name} has no home directory"
            )));
        }
        Ok(Self {
            name: (*name).to_owned(),
            uid: (*uid).to_owned(),
            gid: (*gid).to_owned(),
            home: PathBuf::from(home),
        })
    }
}

pub fn install(target: &Target, start: bool, recovery_key: Option<&str>) -> Result<()> {
    #[cfg(target_os = "linux")]
    guard_scope(target)?;
    if recovery_key.is_some() && target.scope != Scope::System {
        return Err(DaemonError::Invalid(
            "a recovery key is only accepted when provisioning a system service".into(),
        ));
    }
    let was_running = status(target)?;
    // Stop stale service wrappers as well as a currently locked daemon before
    // replacing binaries. This matters on Windows between restart attempts.
    stop(target)?;
    let paths = &target.paths;
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
    #[cfg(target_os = "linux")]
    if let Some(account) = &target.account {
        // Everything under the data home is written here as root; the daemon
        // and the CLI both run as the owning account afterwards.
        chown_recursive(&paths.home, account)?;
        // The data home is not on anyone's PATH, so a system installation that
        // stopped here would leave `hsin` unusable for the very account it was
        // installed for.
        link_into_path(&bin)?;
    }
    install_definition(target, &bin.join(exe_name("hsind")), recovery_key)?;
    if start || was_running {
        self::start(target)?;
    }
    Ok(())
}

pub fn uninstall(target: &Target, purge: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    guard_scope(target)?;
    let paths = &target.paths;
    stop(target)?;
    #[cfg(target_os = "macos")]
    uninstall_definition(paths)?;
    #[cfg(any(target_os = "linux", windows))]
    uninstall_definition(target);
    let _ = fs::remove_file(paths.home.join("bin").join(exe_name("hsind")));
    let _ = fs::remove_file(paths.home.join("bin").join(exe_name("hsin")));
    let _ = fs::remove_file(paths.home.join("bin/run-hsind.cmd"));
    #[cfg(target_os = "linux")]
    if target.scope == Scope::System {
        unlink_from_path(&paths.home.join("bin"));
    }
    if purge {
        #[cfg(target_os = "linux")]
        if target.scope == Scope::System {
            // Sealed credentials live outside the data home and would otherwise
            // survive a purge, unlocking a restored database.
            let _ = fs::remove_dir_all(credential_directory(paths));
        }
        if target.scope == Scope::User {
            let store = SystemKeyStore::for_home(&paths.home);
            for version in stored_key_versions(&paths.database)? {
                store.delete(version)?;
            }
        }
        if paths.home.exists() {
            fs::remove_dir_all(&paths.home)?;
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

pub fn start(target: &Target) -> Result<()> {
    let paths = &target.paths;
    if instance_running(paths)? {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let label = service_label(paths);
        let domain = format!("gui/{}", uid()?);
        let plist = launch_agent_path(paths)?;
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
        let unit = service_unit(paths);
        if target.scope == Scope::System {
            command(Command::new("systemctl").arg("daemon-reload"))?;
            command(Command::new("systemctl").args(["enable", "--now", &unit]))?;
        } else if systemd_user_available() {
            command(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            command(Command::new("systemctl").args(["--user", "enable", "--now", &unit]))?;
        } else {
            spawn_fallback(paths)?;
        }
    }
    #[cfg(windows)]
    {
        command(Command::new("schtasks").args(["/Run", "/TN", &service_label(paths)]))?;
    }
    // `systemctl start` returns success for a Type=simple unit as soon as the
    // fork succeeds, so a daemon that dies during startup still looks installed.
    // The instance lock is the only evidence that it is actually up.
    wait_for_instance_start(paths)
}

pub fn stop(target: &Target) -> Result<()> {
    let paths = &target.paths;
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", uid()?);
        let plist = launch_agent_path(paths)?;
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &plist.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let unit = service_unit(paths);
        if target.scope == Scope::System {
            let _ = Command::new("systemctl")
                .args(["stop", &unit])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        } else {
            if systemd_user_available() {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", &unit])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            stop_fallback(paths)?;
        }
    }
    #[cfg(windows)]
    {
        let label = service_label(paths);
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", &label])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    wait_for_instance_stop(paths)
}

pub fn restart(target: &Target) -> Result<()> {
    stop(target)?;
    start(target)
}

pub fn status(target: &Target) -> Result<bool> {
    instance_running(&target.paths)
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

fn wait_for_instance_start(paths: &Paths) -> Result<()> {
    for _ in 0..50 {
        if instance_running(paths)? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(DaemonError::Internal(
        "daemon did not start before the service timeout; check the service logs".into(),
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
fn install_definition(target: &Target, daemon: &Path, _recovery_key: Option<&str>) -> Result<()> {
    let paths = &target.paths;
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
fn install_definition(target: &Target, daemon: &Path, recovery_key: Option<&str>) -> Result<()> {
    let paths = &target.paths;
    if target.scope == Scope::System {
        return install_system_definition(target, daemon, recovery_key);
    }
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

/// Write the system unit and seal the master key systemd will hand back at
/// runtime. A system unit has no session bus, so the Secret Service the
/// per-user installation relies on cannot be reached from it.
#[cfg(target_os = "linux")]
fn install_system_definition(
    target: &Target,
    daemon: &Path,
    recovery_key: Option<&str>,
) -> Result<()> {
    let paths = &target.paths;
    let account = target
        .account
        .as_ref()
        .ok_or_else(|| DaemonError::Internal("system scope resolved without an account".into()))?;
    let (version, key) = provision_credential(paths, recovery_key)?;
    let credential = crate::crypto::CredentialKeyStore::credential_name(version);
    let mut environment = String::new();
    for (name, value) in service_environment(paths)? {
        let assignment = format!("{name}={value}");
        writeln!(
            &mut environment,
            "Environment={}",
            systemd_quote(&assignment)
        )
        .expect("writing to a String cannot fail");
    }
    let path = system_unit_path(paths);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Only `ExecStart=` and `Environment=` take shell-style quoting. `User=`,
    // `Group=` and the `id:path` pair of `LoadCredentialEncrypted=` are literal
    // strings, so quoting them makes systemd look for an account whose name
    // contains the quote characters and fail the unit with 217/USER.
    fs::write(
        &path,
        format!(
            "[Unit]\nDescription=hsin provider daemon\nAfter=network.target\n\n\
             [Service]\nType=simple\nUser={}\nGroup={}\nExecStart={} run\nRestart=on-failure\n\
             LoadCredentialEncrypted={credential}:{}\n{environment}\n\
             [Install]\nWantedBy=multi-user.target\n",
            account.name,
            account.gid,
            systemd_quote(&daemon.to_string_lossy()),
            key.display(),
        ),
    )?;
    Ok(())
}

/// Seal a master key into `/etc/hsin/<scope>` unless one is already there.
/// Returns the key version and the sealed file so the unit can load it.
#[cfg(target_os = "linux")]
fn provision_credential(paths: &Paths, recovery_key: Option<&str>) -> Result<(u32, PathBuf)> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use zeroize::Zeroizing;

    let directory = credential_directory(paths);
    fs::create_dir_all(&directory)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }

    let (version, secret) = if let Some(recovery) = recovery_key {
        crate::crypto::recovery_key_material(recovery)?
    } else {
        let mut key = [0_u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
        let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(key));
        zeroize::Zeroize::zeroize(&mut key);
        (1, encoded)
    };
    let name = crate::crypto::CredentialKeyStore::credential_name(version);
    let sealed = directory.join(format!("{name}.cred"));
    if sealed.exists() {
        if recovery_key.is_some() {
            return Err(DaemonError::Conflict(format!(
                "{} already holds a sealed master key; remove it before importing another",
                sealed.display()
            )));
        }
        // Reinstalling over a working deployment must not strand the database.
        return Ok((version, sealed));
    }
    seal_credential(&name, &sealed, &secret)?;
    Ok((version, sealed))
}

/// `systemd-creds encrypt` binds the ciphertext to this host (and to the TPM
/// when one is available), so the key never sits on disk in the clear.
#[cfg(target_os = "linux")]
fn seal_credential(name: &str, sealed: &Path, secret: &str) -> Result<()> {
    use std::io::Write as _;

    let mut child = Command::new("systemd-creds")
        .arg(format!("--name={name}"))
        .arg("encrypt")
        .arg("-")
        .arg(sealed)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            DaemonError::Internal(format!(
                "systemd-creds is required for system installation: {error}"
            ))
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| DaemonError::Internal("systemd-creds stdin was not captured".into()))?
        .write_all(secret.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(DaemonError::Internal(format!(
            "systemd-creds encrypt exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(sealed, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn credential_directory(paths: &Paths) -> PathBuf {
    PathBuf::from("/etc/hsin").join(hsin_ipc::home_scope(&paths.home))
}

#[cfg(target_os = "linux")]
fn system_unit_path(paths: &Paths) -> PathBuf {
    PathBuf::from("/etc/systemd/system").join(service_unit(paths))
}

#[cfg(target_os = "linux")]
fn require_root(action: &str) -> Result<()> {
    if uid()? == "0" {
        return Ok(());
    }
    Err(DaemonError::Invalid(format!(
        "{action} requires root; re-run with sudo"
    )))
}

/// Reject scope mismatches before anything is written. `hsin daemon update`
/// reinstalls in user scope, which on a system-managed home would leave a
/// second unit fighting the first one over the same database.
#[cfg(target_os = "linux")]
fn guard_scope(target: &Target) -> Result<()> {
    if target.scope == Scope::System {
        return require_root("changing a system service");
    }
    if system_unit_path(&target.paths).exists() {
        return Err(DaemonError::Conflict(format!(
            "{} is managed by a system unit; re-run as root with --system",
            target.paths.home.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn chown_recursive(path: &Path, account: &Account) -> Result<()> {
    command(
        Command::new("chown")
            .arg("-R")
            .arg(format!("{}:{}", account.uid, account.gid))
            .arg(path),
    )
}

#[cfg(target_os = "linux")]
const PATH_LINK_DIR: &str = "/usr/local/bin";

/// Expose the installed binaries on the default login PATH.
///
/// `current_exe()` resolves through the symlink, so the daemon still finds its
/// own installation home and the portable-install marker keeps working.
#[cfg(target_os = "linux")]
fn link_into_path(bin: &Path) -> Result<()> {
    link_into(Path::new(PATH_LINK_DIR), bin)
}

#[cfg(target_os = "linux")]
fn link_into(directory: &Path, bin: &Path) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for name in ["hsin", "hsind"] {
        let source = bin.join(name);
        if !source.exists() {
            continue;
        }
        let link = directory.join(name);
        // A real file there belongs to some other installation (a package, a
        // manual copy). Replacing it would hijack that install.
        if link.exists() && !link.is_symlink() {
            tracing::warn!(path = %link.display(), "left an existing binary in place");
            continue;
        }
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&source, &link)?;
    }
    Ok(())
}

/// Remove only the links this installation owns, so a second instance or an
/// unrelated package keeps working.
#[cfg(target_os = "linux")]
fn unlink_from_path(bin: &Path) {
    unlink_from(Path::new(PATH_LINK_DIR), bin);
}

#[cfg(target_os = "linux")]
fn unlink_from(directory: &Path, bin: &Path) {
    for name in ["hsin", "hsind"] {
        let link = directory.join(name);
        if fs::read_link(&link).is_ok_and(|actual| actual == bin.join(name)) {
            let _ = fs::remove_file(&link);
        }
    }
}

#[cfg(windows)]
fn install_definition(target: &Target, daemon: &Path, _recovery_key: Option<&str>) -> Result<()> {
    let paths = &target.paths;
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
fn uninstall_definition(target: &Target) {
    let paths = &target.paths;
    let unit = service_unit(paths);
    if target.scope == Scope::System {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", &unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_file(system_unit_path(paths));
        let _ = Command::new("systemctl")
            .arg("daemon-reload")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return;
    }
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
fn uninstall_definition(target: &Target) {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", &service_label(&target.paths)])
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
fn spawn_fallback(paths: &Paths) -> Result<()> {
    if fallback_status(paths)? {
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
    fs::write(fallback_pid_path(paths), child.id().to_string())?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn stop_fallback(paths: &Paths) -> Result<()> {
    let pid_path = fallback_pid_path(paths);
    let Some(pid) = read_fallback_pid(&pid_path)? else {
        return Ok(());
    };
    if !fallback_process_matches(pid, paths) {
        let _ = fs::remove_file(pid_path);
        return Ok(());
    }
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    for _ in 0..50 {
        if !fallback_process_matches(pid, paths) {
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
fn fallback_status(paths: &Paths) -> Result<bool> {
    let pid_path = fallback_pid_path(paths);
    let Some(pid) = read_fallback_pid(&pid_path)? else {
        return Ok(false);
    };
    if fallback_process_matches(pid, paths) {
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
    let mut variables = vec![("HSIN_HOME", absolute_path(&paths.home)?)];
    for key in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            variables.push((key, absolute_path(&PathBuf::from(value))?));
        }
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
    fn instance_status_tracks_the_instance_lock() {
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

    /// A system installation runs as root but must own the target account's
    /// data home. Reading the wrong home would install the service against
    /// root's database while the CLI kept talking to the user's own.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_system_target_follows_the_owning_account_home() {
        let account = Account::parse("alice:x:1000:1000:Alice:/home/alice:/bin/sh\n").unwrap();
        assert_eq!(account.name, "alice");
        assert_eq!(account.uid, "1000");
        assert_eq!(account.gid, "1000");
        assert_eq!(account.home, PathBuf::from("/home/alice"));
        assert_eq!(
            hsin_ipc::data_home_for_account(&account.home),
            PathBuf::from("/home/alice/.local/share").join(hsin_ipc::DATA_DIR_NAME)
        );
    }

    /// Service accounts created with `--no-create-home` cannot hold a data
    /// home, and silently falling back to root's would put the database and the
    /// sealed credential under mismatched ownership.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_account_without_a_home_is_rejected() {
        assert!(Account::parse("daemon:x:1:1:daemon::/usr/sbin/nologin").is_err());
    }

    /// System scope needs an explicit owner. Defaulting to root would install a
    /// service that manages root's Codex and Claude configuration instead of
    /// the operator's.
    #[test]
    fn system_scope_without_an_account_is_rejected() {
        let error = Target::resolve(Scope::System, None);
        // `SUDO_USER` is set when a developer runs the suite under sudo, which
        // makes the resolution legitimately succeed.
        if std::env::var_os("SUDO_USER").is_none() {
            assert!(error.is_err());
        }
    }

    /// The data home is not on PATH, so a system installation links the
    /// binaries into it. Both halves must respect installations that are not
    /// ours: overwriting a packaged `hsin` would hijack it, and removing a link
    /// pointing elsewhere would break it.
    #[cfg(target_os = "linux")]
    #[test]
    fn path_links_never_touch_another_installation() {
        let root = std::env::temp_dir().join(format!("hsin-link-{}", uuid::Uuid::new_v4()));
        let bin = root.join("bin");
        let path_dir = root.join("usr-local-bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(bin.join("hsin"), b"binary").unwrap();
        fs::write(bin.join("hsind"), b"binary").unwrap();

        // A real file from another install must survive.
        fs::write(path_dir.join("hsin"), b"packaged").unwrap();
        link_into(&path_dir, &bin).unwrap();
        assert_eq!(fs::read(path_dir.join("hsin")).unwrap(), b"packaged");
        assert_eq!(
            fs::read_link(path_dir.join("hsind")).unwrap(),
            bin.join("hsind")
        );

        // A link owned by a different home must survive the uninstall.
        let other = root.join("other-home/bin");
        fs::create_dir_all(&other).unwrap();
        fs::remove_file(path_dir.join("hsin")).unwrap();
        std::os::unix::fs::symlink(other.join("hsin"), path_dir.join("hsin")).unwrap();
        unlink_from(&path_dir, &bin);
        assert!(path_dir.join("hsin").is_symlink(), "foreign link kept");
        assert!(!path_dir.join("hsind").exists(), "own link removed");

        fs::remove_dir_all(root).ok();
    }
}
