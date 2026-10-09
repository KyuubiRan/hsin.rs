//! Shared, non-secret ownership records for client configuration directories.
//!
//! One directory lock protects every target in that directory. Callers must
//! acquire directory locks in path order and reuse the guard when two clients
//! use the same directory. Locks never wait, including on a single-threaded
//! Tokio runtime; a busy directory is an ordinary retryable conflict.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;
use hsin_core::{ClientKind, ConfigOwnerInfo, ConfigOwnershipStatus};
use hsin_ipc::IpcEndpoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DaemonError, Result};

const FORMAT_VERSION: u32 = 2;
const RECORD_NAME: &str = ".hsin-config-owner.json";
const LOCK_NAME: &str = ".hsin-config-owner.lock";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Target {
    pub client: ClientKind,
    pub id: String,
    /// The normalized real directory that owns the record and transaction lock.
    pub path: PathBuf,
    pub config_path: PathBuf,
    original_path: PathBuf,
}

impl Target {
    /// Resolves existing symlinks without creating the configuration directory.
    pub fn new(client: ClientKind, config_path: impl AsRef<Path>) -> Result<Self> {
        let original_path = absolute_path(config_path.as_ref())?;
        let config_path = normalize_real_path(&original_path)?;
        let path = config_path
            .parent()
            .ok_or_else(|| DaemonError::Config("client configuration has no directory".into()))?
            .to_owned();
        let mut digest = Sha256::new();
        digest.update(client.as_str().as_bytes());
        digest.update([0]);
        digest.update(config_path.as_os_str().as_encoded_bytes());
        Ok(Self {
            client,
            id: hex::encode(digest.finalize()),
            path,
            config_path,
            original_path,
        })
    }

    /// Acquires the directory's transaction lock without blocking the runtime.
    pub fn lock(&self) -> Result<Guard> {
        self.verify_path()?;
        fs::create_dir_all(&self.path)?;
        self.verify_path()?;
        let lock_path = self.path.join(LOCK_NAME);
        reject_symlink(&lock_path)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(lock_path)?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                return Err(DaemonError::Conflict(
                    "another configuration transaction is in progress; retry the operation".into(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        set_private_file(&lock)?;
        self.verify_path()?;
        let record_path = self.path.join(RECORD_NAME);
        let (document, expected_hash) = read_document(&record_path)?;
        Ok(Guard {
            _lock: lock,
            path: self.path.clone(),
            target_id: self.id.clone(),
            document,
            expected_hash,
        })
    }

    /// Reading status never creates a directory or lock file.
    pub fn read_record(&self) -> Result<Option<Record>> {
        self.verify_path()?;
        let (mut document, _) = read_document(&self.path.join(RECORD_NAME))?;
        Ok(document.targets.remove(&self.id))
    }

    /// The caller supplies any fingerprint drift or legacy-recovery restriction.
    pub fn status(
        &self,
        self_owner: &ConfigOwnerInfo,
        unavailable_reason: Option<String>,
    ) -> Result<ConfigOwnershipStatus> {
        let record = self.read_record()?;
        let owner = record.as_ref().and_then(|record| record.owner.clone());
        let owner_is_self = owner
            .as_ref()
            .is_some_and(|owner| owner.instance_id == self_owner.instance_id);
        let pending = record.as_ref().and_then(|record| record.pending.as_ref());
        let unavailable_reason = unavailable_reason.or_else(|| {
            if pending.is_some() {
                Some("a configuration transaction requires recovery".into())
            } else if owner.is_some()
                && !owner_is_self
                && record
                    .as_ref()
                    .is_some_and(|record| record.endpoint.is_none())
            {
                Some("the managing instance has no compatible IPC endpoint".into())
            } else {
                None
            }
        });
        Ok(ConfigOwnershipStatus {
            client: self.client,
            target_id: self.id.clone(),
            config_path: self.config_path.to_string_lossy().into_owned(),
            generation: record.as_ref().map_or(0, |record| record.generation),
            takeover_available: owner.is_some() && !owner_is_self && unavailable_reason.is_none(),
            owner,
            owner_is_self,
            takeover_unavailable_reason: unavailable_reason,
        })
    }

    fn verify_path(&self) -> Result<()> {
        if normalize_real_path(&self.original_path)? != self.config_path {
            return Err(DaemonError::Conflict(
                "the client configuration path changed; refresh configuration status".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_auth_store: Option<String>,
    #[serde(default)]
    pub codex_auth: bool,
    #[serde(default)]
    pub codex_keys: Vec<String>,
    #[serde(default)]
    pub claude_model_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_auth: Option<String>,
    pub config: String,
    pub auth: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    Write,
    Release,
    Takeover,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
    pub kind: PendingKind,
    pub requester: Option<ConfigOwnerInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub owner: Option<ConfigOwnerInfo>,
    pub generation: u64,
    pub endpoint: Option<IpcEndpoint>,
    pub scope: ManagedScope,
    pub fingerprints: Fingerprints,
    pub pending: Option<Pending>,
}

impl Record {
    pub fn unclaimed(generation: u64, scope: ManagedScope, fingerprints: Fingerprints) -> Self {
        Self {
            owner: None,
            generation,
            endpoint: None,
            scope,
            fingerprints,
            pending: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Document {
    version: u32,
    targets: BTreeMap<String, Record>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            targets: BTreeMap::new(),
        }
    }
}

pub struct Guard {
    _lock: File,
    path: PathBuf,
    target_id: String,
    document: Document,
    expected_hash: Option<String>,
}

impl Guard {
    pub fn record(&self) -> Option<&Record> {
        self.document.targets.get(&self.target_id)
    }

    pub fn record_for(&self, target: &Target) -> Option<&Record> {
        (target.path == self.path)
            .then(|| self.document.targets.get(&target.id))
            .flatten()
    }

    pub fn set_record(&mut self, record: Record) -> Result<()> {
        self.write_record(self.target_id.clone(), record)
    }

    pub fn set_record_for(&mut self, target: &Target, record: Record) -> Result<()> {
        if target.path != self.path {
            return Err(DaemonError::Conflict(
                "the configuration transaction lock belongs to a different directory".into(),
            ));
        }
        target.verify_path()?;
        self.write_record(target.id.clone(), record)
    }

    fn write_record(&mut self, target_id: String, record: Record) -> Result<()> {
        validate_record(&record)?;
        let mut document = self.document.clone();
        document.version = FORMAT_VERSION;
        document.targets.insert(target_id, record);
        let bytes = serde_json::to_vec(&document)?;
        if bytes.len() > usize::try_from(MAX_RECORD_BYTES).unwrap_or(usize::MAX) {
            return Err(invalid_record(
                "configuration ownership record is too large",
            ));
        }
        let record_path = self.path.join(RECORD_NAME);
        let (_, before_hash) = read_document(&record_path)?;
        if before_hash != self.expected_hash {
            return Err(invalid_record(
                "configuration ownership changed outside the transaction",
            ));
        }
        let mut output = AtomicWriteFile::open(&record_path)?;
        #[cfg(unix)]
        set_private_file(output.as_file())?;
        output.write_all(&bytes)?;
        let (_, latest_hash) = read_document(&record_path)?;
        if latest_hash != before_hash {
            return Err(invalid_record(
                "configuration ownership changed while the transaction was writing",
            ));
        }
        output.commit()?;
        #[cfg(unix)]
        File::open(&self.path)?.sync_all()?;
        self.document = document;
        self.expected_hash = Some(hash(&bytes));
        Ok(())
    }
}

fn read_document(path: &Path) -> Result<(Document, Option<String>)> {
    reject_symlink(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Document::default(), None));
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(invalid_record(
            "configuration ownership record is not a file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() > usize::try_from(MAX_RECORD_BYTES).unwrap_or(usize::MAX) {
        return Err(invalid_record(
            "configuration ownership record is too large",
        ));
    }
    let document: Document = serde_json::from_slice(&bytes).map_err(|_| {
        invalid_record("configuration ownership record is damaged; repair is required")
    })?;
    if !(1..=FORMAT_VERSION).contains(&document.version) {
        return Err(invalid_record(
            "configuration ownership record uses an unsupported version; upgrade is required",
        ));
    }
    for (target_id, record) in &document.targets {
        if !is_fingerprint(target_id) {
            return Err(invalid_record(
                "configuration ownership target identifier is invalid",
            ));
        }
        validate_record(record)?;
    }
    Ok((document, Some(hash(&bytes))))
}

fn validate_record(record: &Record) -> Result<()> {
    if (!record.fingerprints.config.is_empty() && !is_fingerprint(&record.fingerprints.config))
        || record
            .fingerprints
            .auth
            .as_deref()
            .is_some_and(|value| !is_fingerprint(value))
        || record
            .fingerprints
            .official_auth
            .as_deref()
            .is_some_and(|value| !is_fingerprint(value))
        || record
            .scope
            .official_auth_store
            .as_deref()
            .is_some_and(|value| !is_fingerprint(value))
    {
        return Err(invalid_record(
            "configuration ownership fingerprints must contain only SHA-256 digests",
        ));
    }
    if record.scope.claude_model_keys.iter().any(|key| {
        !hsin_core::CLAUDE_MODEL_ENV_KEYS
            .iter()
            .any(|allowed| *allowed == key)
    }) {
        return Err(invalid_record(
            "configuration ownership contains an unsupported model-mapping key",
        ));
    }
    Ok(())
}

fn invalid_record(message: &str) -> DaemonError {
    DaemonError::Conflict(message.into())
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_record(
            "configuration ownership metadata must not be a symbolic link",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Compares two spellings of a path by the real location they resolve to.
///
/// Releases before configuration ownership recorded client paths as the
/// environment spelled them; current releases record the normalized real path,
/// which on Windows carries the verbatim `\\?\` prefix.
pub fn same_real_path(left: &Path, right: &Path) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    Ok(normalize_real_path(&absolute_path(left)?)? == normalize_real_path(&absolute_path(right)?)?)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Canonicalizes the nearest existing ancestor, then normalizes absent children.
/// Canonicalization precedes lexical cleanup, so `symlink/..` has its real meaning.
fn normalize_real_path(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path.to_owned();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(normalize_components(&resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor
                    .components()
                    .next_back()
                    .ok_or_else(|| DaemonError::Config("configuration path is invalid".into()))?;
                missing.push(component.as_os_str().to_owned());
                if !ancestor.pop() {
                    return Err(DaemonError::Config("configuration path is invalid".into()));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hsin-ownership-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn target(&self, client: ClientKind) -> Target {
            let file = match client {
                ClientKind::Codex => "config.toml",
                ClientKind::Claude => "settings.json",
            };
            Target::new(client, self.0.join(file)).unwrap()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn owner(id: &str) -> ConfigOwnerInfo {
        ConfigOwnerInfo {
            instance_id: id.into(),
            instance_home: "/instance/home".into(),
            instance_label: "Debug".into(),
            daemon_version: "0.2.8".into(),
        }
    }

    fn record(generation: u64) -> Record {
        Record {
            owner: Some(owner("original")),
            generation,
            endpoint: Some(IpcEndpoint::namespaced("test-owner")),
            scope: ManagedScope::default(),
            fingerprints: Fingerprints {
                official_auth: None,
                config: hash(b"managed-fields"),
                auth: None,
            },
            pending: None,
        }
    }

    #[test]
    fn read_status_does_not_create_missing_directories() {
        let directory = TestDirectory::new();
        let missing = directory.0.join("missing").join("subdirectory");
        let target = Target::new(ClientKind::Codex, missing.join("config.toml")).unwrap();
        assert!(target.read_record().unwrap().is_none());
        let status = target.status(&owner("self"), None).unwrap();
        assert_eq!(status.generation, 0);
        assert!(!status.takeover_available);
        assert!(!missing.exists());
    }

    #[test]
    fn legacy_records_upgrade_before_adding_official_identity_scope() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let legacy = serde_json::json!({"version":1,"targets":{target.id.clone():record(1)}});
        fs::write(
            target.path.join(RECORD_NAME),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        assert!(
            target
                .read_record()
                .unwrap()
                .unwrap()
                .scope
                .official_auth_store
                .is_none()
        );
        let mut updated = target.read_record().unwrap().unwrap();
        updated.scope.official_auth_store = Some(hash(b"native store"));
        updated.fingerprints.official_auth = Some(hash(b"official identity"));
        target.lock().unwrap().set_record(updated.clone()).unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&fs::read(target.path.join(RECORD_NAME)).unwrap()).unwrap();
        assert_eq!(document["version"], FORMAT_VERSION);
        assert_eq!(target.read_record().unwrap().unwrap(), updated);
    }

    #[test]
    fn directory_lock_is_nonblocking_and_survives_guard_drop() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let guard = target.lock().unwrap();
        assert!(matches!(target.lock(), Err(DaemonError::Conflict(_))));
        drop(guard);
        assert!(target.path.join(LOCK_NAME).exists());
        assert!(target.lock().is_ok());
    }

    #[test]
    fn one_directory_guard_preserves_both_client_records() {
        let directory = TestDirectory::new();
        let codex = directory.target(ClientKind::Codex);
        let claude = directory.target(ClientKind::Claude);
        assert_ne!(codex.id, claude.id);
        let mut guard = codex.lock().unwrap();
        guard.set_record(record(1)).unwrap();
        guard.set_record_for(&claude, record(7)).unwrap();
        assert_eq!(guard.record().unwrap().generation, 1);
        assert_eq!(guard.record_for(&claude).unwrap().generation, 7);
        drop(guard);
        assert_eq!(codex.read_record().unwrap().unwrap().generation, 1);
        assert_eq!(claude.read_record().unwrap().unwrap().generation, 7);
    }

    #[test]
    fn guard_rejects_another_directory() {
        let directory = TestDirectory::new();
        let other = TestDirectory::new();
        let mut guard = directory.target(ClientKind::Codex).lock().unwrap();
        let other_target = other.target(ClientKind::Claude);
        assert!(guard.record_for(&other_target).is_none());
        assert!(matches!(
            guard.set_record_for(&other_target, record(1)),
            Err(DaemonError::Conflict(_))
        ));
    }

    #[test]
    fn pending_transaction_and_generation_survive_reopening() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let mut record = record(41);
        record.pending = Some(Pending {
            request_id: uuid::Uuid::new_v4().to_string(),
            kind: PendingKind::Release,
            requester: Some(owner("recipient")),
        });
        target.lock().unwrap().set_record(record.clone()).unwrap();
        assert_eq!(target.read_record().unwrap().unwrap(), record);
        let status = target.status(&owner("recipient"), None).unwrap();
        assert!(!status.takeover_available);
        assert!(status.takeover_unavailable_reason.is_some());
    }

    #[test]
    fn damaged_and_future_records_do_not_become_unmanaged() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        for contents in ["not json", "{\"version\":3,\"targets\":{}}"] {
            fs::write(target.path.join(RECORD_NAME), contents).unwrap();
            assert!(matches!(
                target.read_record(),
                Err(DaemonError::Conflict(_))
            ));
            assert!(matches!(target.lock(), Err(DaemonError::Conflict(_))));
        }
    }

    #[test]
    fn raw_configuration_cannot_be_written_as_a_fingerprint() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let mut record = record(1);
        record.fingerprints.config = "OPENAI_API_KEY=secret-credential".into();
        assert!(matches!(
            target.lock().unwrap().set_record(record),
            Err(DaemonError::Conflict(_))
        ));
        assert!(!target.path.join(RECORD_NAME).exists());
    }

    #[test]
    fn fingerprints_persist_without_raw_secret_material() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let mut record = record(1);
        record.scope.codex_auth = true;
        record.fingerprints.auth = Some(hash(b"secret-credential"));
        target.lock().unwrap().set_record(record).unwrap();
        let contents = fs::read_to_string(target.path.join(RECORD_NAME)).unwrap();
        assert!(!contents.contains("secret-credential"));
        assert!(!contents.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn guard_rejects_external_record_modification() {
        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        let mut guard = target.lock().unwrap();
        guard.set_record(record(1)).unwrap();
        fs::write(
            target.path.join(RECORD_NAME),
            "{\"version\":1,\"targets\":{}}",
        )
        .unwrap();
        assert!(matches!(
            guard.set_record(record(2)),
            Err(DaemonError::Conflict(_))
        ));
        assert_eq!(guard.record().unwrap().generation, 1);
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_file_symlink_aliases_share_target_identity() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let real = directory.0.join("real");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, directory.0.join("alias")).unwrap();
        let actual = Target::new(ClientKind::Codex, real.join("config.toml")).unwrap();
        let alias = Target::new(
            ClientKind::Codex,
            directory.0.join("alias").join("config.toml"),
        )
        .unwrap();
        assert_eq!(actual.id, alias.id);
        actual.lock().unwrap().set_record(record(1)).unwrap();
        assert_eq!(alias.read_record().unwrap().unwrap().generation, 1);

        fs::write(real.join("config.toml"), "model_provider = 'OpenAI'").unwrap();
        symlink(real.join("config.toml"), directory.0.join("linked.toml")).unwrap();
        let linked = Target::new(ClientKind::Codex, directory.0.join("linked.toml")).unwrap();
        assert_eq!(actual.id, linked.id);
    }

    #[cfg(unix)]
    #[test]
    fn missing_children_resolve_symlink_parent_before_dotdot() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        fs::create_dir_all(directory.0.join("real").join("nested")).unwrap();
        symlink(
            directory.0.join("real").join("nested"),
            directory.0.join("alias"),
        )
        .unwrap();
        let alias = Target::new(
            ClientKind::Codex,
            directory
                .0
                .join("alias")
                .join("..")
                .join("new")
                .join("config.toml"),
        )
        .unwrap();
        let actual = Target::new(
            ClientKind::Codex,
            directory.0.join("real").join("new").join("config.toml"),
        )
        .unwrap();
        assert_eq!(alias.id, actual.id);
    }

    #[cfg(unix)]
    #[test]
    fn target_rejects_changed_symlink_resolution() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        fs::create_dir_all(directory.0.join("one")).unwrap();
        fs::create_dir_all(directory.0.join("two")).unwrap();
        let alias = directory.0.join("alias");
        symlink(directory.0.join("one"), &alias).unwrap();
        let target = Target::new(ClientKind::Codex, alias.join("config.toml")).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(directory.0.join("two"), &alias).unwrap();
        assert!(matches!(target.lock(), Err(DaemonError::Conflict(_))));
        assert!(matches!(
            target.read_record(),
            Err(DaemonError::Conflict(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn metadata_is_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TestDirectory::new();
        let target = directory.target(ClientKind::Codex);
        target.lock().unwrap().set_record(record(1)).unwrap();
        for name in [RECORD_NAME, LOCK_NAME] {
            assert_eq!(
                fs::metadata(target.path.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let other = directory.0.join("other");
        fs::write(&other, "{\"version\":1,\"targets\":{}}").unwrap();
        fs::remove_file(target.path.join(RECORD_NAME)).unwrap();
        symlink(&other, target.path.join(RECORD_NAME)).unwrap();
        assert!(matches!(
            target.read_record(),
            Err(DaemonError::Conflict(_))
        ));
        assert!(matches!(target.lock(), Err(DaemonError::Conflict(_))));
    }
}
