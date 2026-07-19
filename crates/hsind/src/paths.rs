use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

use fs2::FileExt;

use crate::error::{DaemonError, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub database: PathBuf,
    pub lock: PathBuf,
    pub logs: PathBuf,
    pub backups: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let home = hsin_ipc::data_home();
        Self {
            database: home.join("hsin.sqlite3"),
            lock: home.join("hsind.lock"),
            logs: home.join("logs"),
            backups: home.join("backups"),
            home,
        }
    }

    pub fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.home)?;
        fs::create_dir_all(&self.logs)?;
        fs::create_dir_all(&self.backups)?;
        #[cfg(unix)]
        set_private_dir(&self.home)?;
        #[cfg(not(unix))]
        set_private_dir(&self.home);
        Ok(())
    }
}

pub struct InstanceGuard {
    _file: File,
}

impl InstanceGuard {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock_exclusive().map_err(|_| {
            DaemonError::Conflict("another hsind instance is already running".into())
        })?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) {}
