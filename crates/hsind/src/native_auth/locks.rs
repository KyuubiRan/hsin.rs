//! Claude Code's proper-lockfile protocol uses empty lock directories, rather
//! than advisory file locks. Coordinate both current and legacy refresh paths.

use std::{
    ffi::OsString,
    fs::{self, File, FileTimes, Metadata, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime},
};

use parking_lot::{Condvar, Mutex};

use super::{DaemonError, Result};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

struct DirectoryLease {
    path: PathBuf,
    file: File,
    identity: Metadata,
}

impl DirectoryLease {
    fn acquire(path: &Path, stale: Duration) -> Result<Self> {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                remove_stale_directory(path, stale)?;
                fs::create_dir(path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        contended()
                    } else {
                        DaemonError::Io(error)
                    }
                })?;
            }
            Err(error) => return Err(DaemonError::Io(error)),
        }
        let created = fs::symlink_metadata(path)?;
        if !created.is_dir() || created.file_type().is_symlink() {
            return Err(contended());
        }
        let opened = open_directory(path).and_then(|file| {
            let identity = file.metadata()?;
            if !same_directory(&created, &identity) {
                return Err(contended());
            }
            Ok(Self {
                path: path.to_path_buf(),
                file,
                identity,
            })
        });
        if opened.is_err()
            && fs::symlink_metadata(path).is_ok_and(|current| {
                current.is_dir()
                    && !current.file_type().is_symlink()
                    && same_directory(&created, &current)
            })
        {
            let _ = fs::remove_dir(path);
        }
        opened
    }

    fn is_owned(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && same_directory(&self.identity, &metadata)
        })
    }

    fn heartbeat(&self) -> Result<()> {
        if !self.is_owned() {
            return Err(contended());
        }
        self.file
            .set_times(FileTimes::new().set_modified(SystemTime::now()))?;
        Ok(())
    }
}

impl Drop for DirectoryLease {
    fn drop(&mut self) {
        // Never remove a directory installed by another process after ours was
        // removed or renamed, and never recursively delete unexpected contents.
        if self.is_owned() {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

struct SharedLocks {
    leases: Vec<DirectoryLease>,
    stopped: Mutex<bool>,
    changed: Condvar,
    compromised: AtomicBool,
}

pub(super) struct NativeDirectoryLocks {
    shared: Arc<SharedLocks>,
    heartbeat: Option<JoinHandle<()>>,
}

impl NativeDirectoryLocks {
    pub(super) fn acquire(home: &Path, metadata: &Path) -> Result<Self> {
        let canonical_home = home.canonicalize()?;
        let paths = [
            (home.join(".oauth_refresh.lock"), Duration::from_secs(60)),
            (append_lock(&canonical_home), Duration::from_secs(60)),
            (home.join(".storage-write.lock"), Duration::from_secs(15)),
            (append_lock(metadata), Duration::from_secs(10)),
        ];
        let mut leases = Vec::with_capacity(paths.len());
        for (path, stale) in paths {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            leases.push(DirectoryLease::acquire(&path, stale)?);
        }
        let shared = Arc::new(SharedLocks {
            leases,
            stopped: Mutex::new(false),
            changed: Condvar::new(),
            compromised: AtomicBool::new(false),
        });
        let worker_state = Arc::clone(&shared);
        let heartbeat = thread::Builder::new()
            .name("native-auth-locks".into())
            .spawn(move || {
                let mut stopped = worker_state.stopped.lock();
                loop {
                    worker_state
                        .changed
                        .wait_for(&mut stopped, HEARTBEAT_INTERVAL);
                    if *stopped {
                        return;
                    }
                    for lease in &worker_state.leases {
                        if lease.heartbeat().is_err() {
                            worker_state.compromised.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            shared,
            heartbeat: Some(heartbeat),
        })
    }

    pub(super) fn check(&self) -> Result<()> {
        if self.shared.compromised.load(Ordering::Acquire)
            || self.shared.leases.iter().any(|lease| !lease.is_owned())
        {
            return Err(contended());
        }
        Ok(())
    }
}

impl Drop for NativeDirectoryLocks {
    fn drop(&mut self) {
        *self.shared.stopped.lock() = true;
        self.shared.changed.notify_one();
        if let Some(worker) = self.heartbeat.take() {
            let _ = worker.join();
        }
        // The last Arc releases only directories whose identity still matches.
    }
}

fn append_lock(path: &Path) -> PathBuf {
    let mut value: OsString = path.as_os_str().into();
    value.push(".lock");
    PathBuf::from(value)
}

fn remove_stale_directory(path: &Path, stale: Duration) -> Result<()> {
    let observed = fs::symlink_metadata(path)?;
    let modified = observed.modified()?;
    if !observed.is_dir()
        || observed.file_type().is_symlink()
        || !SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > stale)
    {
        return Err(contended());
    }
    let current = fs::symlink_metadata(path)?;
    if !current.is_dir()
        || current.file_type().is_symlink()
        || !same_directory(&observed, &current)
        || current.modified()? != modified
    {
        return Err(contended());
    }
    // proper-lockfile recovers expired leases with rmdir. Unexpected contents,
    // symbolic links and a refreshed or replaced lease are never deleted.
    fs::remove_dir(path).map_err(|_| contended())
}

fn open_directory(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS allows opening a directory handle.
        options.custom_flags(0x0200_0000);
        options.write(true);
    }
    Ok(options.open(path)?)
}

fn same_directory(first: &Metadata, second: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.dev() == second.dev() && first.ino() == second.ino()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        first.creation_time() == second.creation_time()
    }
    #[cfg(not(any(unix, windows)))]
    {
        first
            .created()
            .ok()
            .zip(second.created().ok())
            .is_some_and(|(a, b)| a == b)
    }
}

fn contended() -> DaemonError {
    DaemonError::Conflict("native authentication is locked or its switch lock changed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        home: PathBuf,
        paths: [PathBuf; 4],
    }

    impl Fixture {
        fn new() -> Self {
            let home =
                std::env::temp_dir().join(format!("hsin-native-lock-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&home).unwrap();
            let paths = [
                home.join(".oauth_refresh.lock"),
                append_lock(&home.canonicalize().unwrap()),
                home.join(".storage-write.lock"),
                home.join(".claude.json.lock"),
            ];
            Self { home, paths }
        }

        fn acquire(&self) -> Result<NativeDirectoryLocks> {
            NativeDirectoryLocks::acquire(&self.home, &self.home.join(".claude.json"))
        }

        fn make_stale(path: &Path) {
            fs::create_dir(path).unwrap();
            open_directory(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.paths[1]);
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn stale_native_leases_are_recovered_and_released() {
        let fixture = Fixture::new();
        for path in &fixture.paths {
            Fixture::make_stale(path);
        }
        let locks = fixture.acquire().unwrap();
        locks.check().unwrap();
        for path in &fixture.paths {
            assert!(path.is_dir());
            assert!(
                SystemTime::now()
                    .duration_since(fs::metadata(path).unwrap().modified().unwrap())
                    .unwrap()
                    < Duration::from_secs(5)
            );
        }
        drop(locks);
        assert!(fixture.paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn live_native_lease_blocks_without_removing_it() {
        for index in 0..4 {
            let fixture = Fixture::new();
            fs::create_dir(&fixture.paths[index]).unwrap();
            let identity = fs::metadata(&fixture.paths[index]).unwrap();
            assert!(matches!(fixture.acquire(), Err(DaemonError::Conflict(_))));
            assert!(same_directory(
                &identity,
                &fs::metadata(&fixture.paths[index]).unwrap()
            ));
            assert!(fixture.paths[..index].iter().all(|path| !path.exists()));
        }
    }

    #[test]
    fn replaced_lease_is_compromised_and_never_removed() {
        let fixture = Fixture::new();
        let locks = fixture.acquire().unwrap();
        let moved = fixture.home.join("moved-native-lease");
        fs::rename(&fixture.paths[0], &moved).unwrap();
        fs::create_dir(&fixture.paths[0]).unwrap();
        let replacement = fs::metadata(&fixture.paths[0]).unwrap();
        assert!(matches!(locks.check(), Err(DaemonError::Conflict(_))));
        drop(locks);
        assert!(same_directory(
            &replacement,
            &fs::metadata(&fixture.paths[0]).unwrap()
        ));
        assert!(moved.is_dir());
    }

    #[test]
    fn stale_nonempty_native_directory_is_not_recursively_removed() {
        let fixture = Fixture::new();
        Fixture::make_stale(&fixture.paths[0]);
        fs::write(fixture.paths[0].join("unexpected"), "retain").unwrap();
        open_directory(&fixture.paths[0])
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        assert!(matches!(fixture.acquire(), Err(DaemonError::Conflict(_))));
        assert_eq!(
            fs::read_to_string(fixture.paths[0].join("unexpected")).unwrap(),
            "retain"
        );
    }

    #[test]
    fn heartbeat_updates_owned_lease_without_replacing_it() {
        let fixture = Fixture::new();
        let locks = fixture.acquire().unwrap();
        let lease = &locks.shared.leases[0];
        lease
            .file
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        lease.heartbeat().unwrap();
        assert!(lease.is_owned());
        assert_ne!(
            fs::metadata(&lease.path).unwrap().modified().unwrap(),
            SystemTime::UNIX_EPOCH
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_lease_symlink_is_never_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let external = fixture.home.join("external");
        Fixture::make_stale(&external);
        symlink(&external, &fixture.paths[0]).unwrap();
        assert!(matches!(fixture.acquire(), Err(DaemonError::Conflict(_))));
        assert!(
            fs::symlink_metadata(&fixture.paths[0])
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(external.is_dir());
    }
}
