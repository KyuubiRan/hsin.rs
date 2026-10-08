//! Client-native OAuth storage. Credentials never enter public provider DTOs.
//!
//! The caller journals the encrypted before/after snapshots. This module supplies
//! store-wide locking, compare-and-swap and strictly scoped, preserving writes.

use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use atomic_write_file::AtomicWriteFile;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use directories::BaseDirs;
use fs2::FileExt;
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    error::{DaemonError, Result},
    model::ClientKind,
};

#[path = "native_auth/json_patch.rs"]
mod json_patch;
#[path = "native_auth/locks.rs"]
mod locks;

const CODEX_KEYS: &[&str] = &["auth_mode", "OPENAI_API_KEY", "tokens", "last_refresh"];
const CLAUDE_KEYS: &[&str] = &["claudeAiOauth"];
const CLAUDE_METADATA_KEYS: &[&str] = &["oauthAccount"];
const CODEX_AUTH_SERVICE: &str = "Codex Auth";
const CODEX_SECRETS_SERVICE: &str = "codex";
const CODEX_AUTH_ENTRY: &str = "global/CODEX_AUTH";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeIdentity {
    pub account_id: String,
    pub organization_id: String,
    pub email: Option<String>,
    pub native_name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NativeAuthSnapshot {
    pub client: ClientKind,
    pub identity: Option<NativeIdentity>,
    pub auth: Value,
    pub metadata: Value,
}

impl Drop for NativeAuthSnapshot {
    fn drop(&mut self) {
        wipe_json(&mut self.auth);
        wipe_json(&mut self.metadata);
    }
}

fn wipe_json(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(wipe_json),
        Value::Object(values) => values.values_mut().for_each(wipe_json),
        _ => {}
    }
}

struct SecretJson(Value);

impl Drop for SecretJson {
    fn drop(&mut self) {
        wipe_json(&mut self.0);
    }
}

pub trait NativeCredentialStore: Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn store(&self, service: &str, account: &str, value: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

pub(crate) struct SystemNativeCredentialStore;

impl NativeCredentialStore for SystemNativeCredentialStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>> {
        let entry = native_entry(service, account)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(native_keyring_error()),
        }
    }

    fn store(&self, service: &str, account: &str, value: &str) -> Result<()> {
        native_entry(service, account)?
            .set_password(value)
            .map_err(|_| native_keyring_error())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        match native_entry(service, account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(native_keyring_error()),
        }
    }
}

fn native_entry(service: &str, account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, account).map_err(|_| native_keyring_error())
}

fn native_keyring_error() -> DaemonError {
    // Platform errors can contain credential data; never propagate their text.
    DaemonError::Keyring("native client credential storage is unavailable".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodexMode {
    File,
    Keyring,
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyringBackend {
    Direct,
    Secrets,
}

enum Backend {
    Codex {
        mode: CodexMode,
        keyring: KeyringBackend,
    },
    ClaudeFile,
    ClaudeKeychain {
        service: String,
        account: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    File,
    Keyring,
    Secrets,
}

impl Source {
    fn receipt(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Keyring => "keyring",
            Self::Secrets => "secrets",
        }
    }
}

#[derive(Clone)]
struct RawState {
    source: Source,
    auth: Zeroizing<String>,
    metadata: Zeroizing<String>,
    file_auth: Zeroizing<String>,
    secure_available: bool,
}

struct LockState {
    depth: usize,
    file: Option<File>,
    native: Option<locks::NativeDirectoryLocks>,
}

pub struct NativeAuthStore {
    client: ClientKind,
    config_path: PathBuf,
    home: PathBuf,
    auth_path: PathBuf,
    metadata_path: Option<PathBuf>,
    backend: Backend,
    credentials: Arc<dyn NativeCredentialStore>,
    lock_path: PathBuf,
    lock: ReentrantMutex<RefCell<LockState>>,
    id: String,
}

/// A reentrant guard: callers may hold it across capture, journaling and apply.
pub struct NativeAuthSwitchGuard<'a> {
    state: ReentrantMutexGuard<'a, RefCell<LockState>>,
}

impl Drop for NativeAuthSwitchGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.depth -= 1;
        if state.depth == 0
            && let Some(file) = state.file.take()
        {
            state.native.take();
            let _ = FileExt::unlock(&file);
        }
    }
}

impl NativeAuthStore {
    pub fn with_credentials(
        client: ClientKind,
        config_path: &Path,
        claude_explicit_dir: bool,
        credentials: Arc<dyn NativeCredentialStore>,
    ) -> Result<Self> {
        let home = config_path
            .parent()
            .ok_or_else(|| {
                DaemonError::Config("native auth configuration has no directory".into())
            })?
            .to_path_buf();
        let home = if client == ClientKind::Claude {
            // Claude normalizes CLAUDE_CONFIG_DIR before using it as a path, as
            // well as when deriving its secure-storage service suffix.
            PathBuf::from(
                home.to_str()
                    .ok_or_else(native_format_error)?
                    .nfc()
                    .collect::<String>(),
            )
        } else {
            home
        };
        let (backend, auth_path, metadata_path, lock_path, id) = match client {
            ClientKind::Codex => {
                let (mode, keyring) = codex_settings(config_path)?;
                let auth_path = home.join("auth.json");
                let mode_id = match mode {
                    CodexMode::File => "file",
                    CodexMode::Keyring => "keyring",
                    CodexMode::Auto => "auto",
                };
                let backend_id = match keyring {
                    KeyringBackend::Direct => "direct",
                    KeyringBackend::Secrets => "secrets",
                };
                let identity = format!("codex:{mode_id}:{backend_id}:{}", canonical_path(&home));
                (
                    Backend::Codex { mode, keyring },
                    auth_path,
                    None,
                    home.join(".hsin-native-auth.lock"),
                    digest(identity.as_bytes()),
                )
            }
            ClientKind::Claude => {
                let metadata_path = claude_metadata_path(&home, claude_explicit_dir)?;
                if cfg!(target_os = "macos") {
                    let service = claude_service(&home, claude_explicit_dir);
                    let account = claude_account();
                    let keychain_id = format!("claude:keychain:{service}:{account}");
                    // Credential-store locks must be shared even when config paths differ.
                    #[cfg(not(test))]
                    let lock_path = user_home()?
                        .join(".hsin-native-auth-locks")
                        .join(format!("{}.lock", digest(keychain_id.as_bytes())));
                    #[cfg(test)]
                    let lock_path = home.join(".hsin-native-auth.lock");
                    let identity = format!("{keychain_id}:{}", canonical_path(&metadata_path));
                    (
                        Backend::ClaudeKeychain { service, account },
                        home.join(".credentials.json"),
                        Some(metadata_path),
                        lock_path,
                        digest(identity.as_bytes()),
                    )
                } else {
                    let auth_path = home.join(".credentials.json");
                    let identity = format!(
                        "claude:file:{}:{}",
                        canonical_path(&auth_path),
                        canonical_path(&metadata_path)
                    );
                    (
                        Backend::ClaudeFile,
                        auth_path,
                        Some(metadata_path),
                        home.join(".hsin-native-auth.lock"),
                        digest(identity.as_bytes()),
                    )
                }
            }
        };
        Ok(Self {
            client,
            config_path: config_path.to_path_buf(),
            home,
            auth_path,
            metadata_path,
            backend,
            credentials,
            lock_path,
            lock: ReentrantMutex::new(RefCell::new(LockState {
                depth: 0,
                file: None,
                native: None,
            })),
            id,
        })
    }

    pub fn store_id(&self) -> String {
        self.id.clone()
    }

    pub fn lock_for_switch(&self) -> Result<NativeAuthSwitchGuard<'_>> {
        let guard = self.lock.lock();
        {
            let mut state = guard.borrow_mut();
            if state.depth == 0 {
                let parent = self.lock_path.parent().ok_or_else(|| {
                    DaemonError::Config("native auth lock has no directory".into())
                })?;
                fs::create_dir_all(parent)?;
                let mut options = OpenOptions::new();
                options.create(true).truncate(false).read(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let file = options.open(&self.lock_path)?;
                FileExt::try_lock_exclusive(&file).map_err(|error| {
                    // Windows reports ERROR_LOCK_VIOLATION rather than WouldBlock.
                    // Match fs2's native contention code, as ownership locks do.
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                        DaemonError::Conflict("native authentication is being switched".into())
                    } else {
                        DaemonError::Io(error)
                    }
                })?;
                if self.client == ClientKind::Claude {
                    let metadata = self
                        .metadata_path
                        .as_deref()
                        .ok_or_else(native_format_error)?;
                    state.native =
                        Some(locks::NativeDirectoryLocks::acquire(&self.home, metadata)?);
                }
                state.file = Some(file);
            }
            if let Some(native) = &state.native {
                native.check()?;
            }
            state.depth += 1;
        }
        Ok(NativeAuthSwitchGuard { state: guard })
    }

    pub fn capture(&self) -> Result<NativeAuthSnapshot> {
        self.check_backend_configuration()?;
        let raw = self.read_state()?;
        self.snapshot(&raw)
    }

    /// One read supplies the encrypted journal snapshot, CAS digest and public
    /// source receipt. A concurrent refresh cannot separate these three values.
    pub fn capture_for_switch(&self) -> Result<(NativeAuthSnapshot, String, String)> {
        self.check_backend_configuration()?;
        let raw = self.read_state()?;
        self.require_writable(&raw)?;
        Ok((
            self.snapshot(&raw)?,
            self.state_fingerprint(&raw)?,
            raw.source.receipt().into(),
        ))
    }

    /// Stable ownership evidence accepts refreshes of the active account while
    /// protecting its storage source and any inactive file credentials.
    pub fn ownership_fingerprint(&self) -> Result<String> {
        #[derive(Serialize)]
        struct OwnershipState<'a> {
            store: &'a str,
            source: &'static str,
            secure_available: bool,
            identity: Option<(&'a str, &'a str)>,
            auth_mode: Option<&'a Value>,
            api_key: Option<&'a Value>,
            unrecognized_auth: Option<&'a Value>,
            unrecognized_metadata: Option<&'a Value>,
            inactive_fallback: Option<&'a Value>,
        }
        self.check_backend_configuration()?;
        let raw = self.read_state()?;
        let snapshot = self.snapshot(&raw)?;
        let fallback = SecretJson(projection(&raw.file_auth, self.auth_keys())?);
        let identity = snapshot.identity.as_ref().map(|identity| {
            (
                identity.account_id.as_str(),
                identity.organization_id.as_str(),
            )
        });
        let state = OwnershipState {
            store: &self.id,
            source: raw.source.receipt(),
            secure_available: raw.secure_available,
            identity,
            auth_mode: snapshot.auth.get("auth_mode"),
            api_key: snapshot.auth.get("OPENAI_API_KEY"),
            unrecognized_auth: identity.is_none().then_some(&snapshot.auth),
            unrecognized_metadata: identity.is_none().then_some(&snapshot.metadata),
            inactive_fallback: (raw.source != Source::File).then_some(&fallback.0),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&state)?);
        Ok(digest(&bytes))
    }

    /// Immediate CAS digest. Includes all owned native auth and account metadata.
    #[cfg(test)]
    pub fn fingerprint(&self) -> Result<String> {
        self.check_backend_configuration()?;
        self.state_fingerprint(&self.read_state()?)
    }

    pub fn apply(&self, snapshot: &NativeAuthSnapshot, expected_fingerprint: &str) -> Result<()> {
        self.validate_snapshot(snapshot)?;
        let _guard = self.lock_for_switch()?;
        self.check_backend_configuration()?;
        let before = self.read_state()?;
        self.require_writable(&before)?;
        if self.state_fingerprint(&before)? != expected_fingerprint {
            return Err(native_conflict());
        }
        self.apply_state(snapshot, &before, expected_fingerprint, before.source)
    }

    fn apply_state(
        &self,
        snapshot: &NativeAuthSnapshot,
        observed: &RawState,
        expected_fingerprint: &str,
        write_source: Source,
    ) -> Result<()> {
        let mut before = observed.clone();
        if write_source != before.source {
            // An auto-mode secure deletion exposed the inactive file. Rollback
            // writes to the original, now absent secure entry, never that file.
            before.source = write_source;
            before.auth = Zeroizing::new(String::new());
        }
        let after_auth = Zeroizing::new(json_patch::patch(
            &before.auth,
            &snapshot.auth,
            self.auth_keys(),
        )?);
        let after_metadata = Zeroizing::new(json_patch::patch(
            &before.metadata,
            &snapshot.metadata,
            self.metadata_keys(),
        )?);
        // Parse both outputs before committing either resource.
        validate_object(&after_auth)?;
        validate_object(&after_metadata)?;
        // In auto mode, an external login may create the previously empty secure
        // store while the file patch is being prepared.
        if self.state_fingerprint(&self.read_state()?)? != expected_fingerprint {
            return Err(native_conflict());
        }
        self.check_native_lock()?;
        self.write_auth(&before, &after_auth)?;
        if let Some(path) = &self.metadata_path {
            // A multi-resource interruption is recovered from the caller's journal.
            atomic_write(path, &before.metadata, after_metadata.as_bytes())?;
        }
        let applied = self.capture()?;
        self.check_native_lock()?;
        if applied.auth != snapshot.auth || applied.metadata != snapshot.metadata {
            return Err(native_conflict());
        }
        Ok(())
    }

    /// Resume only a journaled before/after state. The prepared digest also proves
    /// that an inactive Codex fallback has not become an external login.
    pub fn recover_apply(
        &self,
        before: &NativeAuthSnapshot,
        after: &NativeAuthSnapshot,
        prepared_before_fingerprint: &str,
    ) -> Result<()> {
        self.validate_snapshot(before)?;
        self.validate_snapshot(after)?;
        let _guard = self.lock_for_switch()?;
        self.check_backend_configuration()?;
        let raw = self.read_state()?;
        self.require_writable(&raw)?;
        let current = self.snapshot(&raw)?;
        let fallback = SecretJson(projection(&raw.file_auth, self.auth_keys())?);
        let completed = current.auth == after.auth && current.metadata == after.metadata;
        if completed
            && (raw.source == Source::File || fallback.0.as_object().is_some_and(Map::is_empty))
        {
            // File applies are atomic, and a completed secure apply has already
            // cleared all owned fallback fields. No resource needs to be written.
            self.check_native_lock()?;
            return Ok(());
        }

        let known_auth = current.auth == before.auth || current.auth == after.auth;
        let known_metadata =
            current.metadata == before.metadata || current.metadata == after.metadata;
        let mut prepared_matches = false;
        if known_auth && known_metadata {
            // In auto/file mode the selected file is also the fallback projection;
            // its atomic replacement changes both projections together.
            let original_fallback = if matches!(
                self.backend,
                Backend::Codex {
                    mode: CodexMode::Auto,
                    ..
                }
            ) && raw.source == Source::File
            {
                &before.auth
            } else {
                &fallback.0
            };
            prepared_matches = self.projected_fingerprint(
                raw.source,
                raw.secure_available,
                &before.auth,
                &before.metadata,
                original_fallback,
            )? == prepared_before_fingerprint;
        }
        if !prepared_matches
            && known_metadata
            && raw.source == Source::File
            && after.auth.as_object().is_some_and(Map::is_empty)
            && let Backend::Codex {
                mode: CodexMode::Auto,
                keyring,
            } = self.backend
        {
            // Deleting the final secure auth entry makes native auto loading fall
            // back to auth.json before cleanup. Prove this exact inactive file
            // against the original secure-store digest before removing its keys.
            let original_source = match keyring {
                KeyringBackend::Direct => Source::Keyring,
                KeyringBackend::Secrets => Source::Secrets,
            };
            prepared_matches = self.projected_fingerprint(
                original_source,
                raw.secure_available,
                &before.auth,
                &before.metadata,
                &fallback.0,
            )? == prepared_before_fingerprint;
        }
        if !prepared_matches {
            return Err(native_conflict());
        }
        let current_fingerprint = self.state_fingerprint(&raw)?;
        self.apply(after, &current_fingerprint)
    }

    /// Restore a journaled switch with the CAS evidence that was prepared for
    /// it. The source receipt keeps auto-mode secure credentials out of files.
    pub fn recover_rollback(
        &self,
        before: &NativeAuthSnapshot,
        after: &NativeAuthSnapshot,
        prepared_before_fingerprint: &str,
        before_source: &str,
    ) -> Result<()> {
        self.validate_snapshot(before)?;
        self.validate_snapshot(after)?;
        let _guard = self.lock_for_switch()?;
        self.check_backend_configuration()?;
        let raw = self.read_state()?;
        self.require_writable(&raw)?;
        let current = self.snapshot(&raw)?;
        let fallback = SecretJson(projection(&raw.file_auth, self.auth_keys())?);
        let source = self.rollback_source(
            &raw,
            before,
            &fallback.0,
            prepared_before_fingerprint,
            before_source,
        )?;
        let deleted_secure = source != Source::File
            && raw.source == Source::File
            && matches!(
                self.backend,
                Backend::Codex {
                    mode: CodexMode::Auto,
                    ..
                }
            )
            && after.auth.as_object().is_some_and(Map::is_empty);
        if source != raw.source && !deleted_secure {
            return Err(native_conflict());
        }
        let known_auth = current.auth == before.auth || current.auth == after.auth;
        let known_metadata =
            current.metadata == before.metadata || current.metadata == after.metadata;
        let prepared_matches = self.rollback_fingerprint(&raw, source, before, &fallback.0)?
            == prepared_before_fingerprint;
        let fallback_cleaned =
            source != Source::File && fallback.0.as_object().is_some_and(Map::is_empty);
        if !known_metadata
            || !(known_auth || deleted_secure && prepared_matches)
            || !(prepared_matches || known_auth && fallback_cleaned)
        {
            return Err(native_conflict());
        }
        if current.auth == before.auth
            && current.metadata == before.metadata
            && raw.source == source
            && (source == Source::File || fallback_cleaned)
        {
            self.check_native_lock()?;
            return Ok(());
        }
        // Keep the exact observed digest through the write; do not accept a new
        // fallback or secure entry by taking a fresh fingerprint here.
        let observed_fingerprint = self.state_fingerprint(&raw)?;
        self.apply_state(before, &raw, &observed_fingerprint, source)
    }

    fn rollback_source(
        &self,
        raw: &RawState,
        before: &NativeAuthSnapshot,
        fallback: &Value,
        prepared_fingerprint: &str,
        receipt: &str,
    ) -> Result<Source> {
        let secure_source = match self.backend {
            Backend::Codex {
                keyring: KeyringBackend::Secrets,
                ..
            } => Source::Secrets,
            _ => Source::Keyring,
        };
        let allowed = match self.backend {
            Backend::ClaudeFile
            | Backend::Codex {
                mode: CodexMode::File,
                ..
            } => vec![Source::File],
            Backend::Codex {
                mode: CodexMode::Auto,
                ..
            } => vec![Source::File, secure_source],
            _ => vec![secure_source],
        };
        if !receipt.is_empty() {
            return allowed
                .into_iter()
                .find(|source| source.receipt() == receipt)
                .ok_or_else(native_conflict);
        }
        if allowed.len() == 1 {
            return Ok(allowed[0]);
        }
        for source in allowed {
            if self.rollback_fingerprint(raw, source, before, fallback)? == prepared_fingerprint {
                return Ok(source);
            }
        }
        // Older journals lack the receipt. A source that cannot be reconstructed
        // from their original digest must not be guessed into plaintext storage.
        Err(native_conflict())
    }

    fn rollback_fingerprint(
        &self,
        raw: &RawState,
        source: Source,
        before: &NativeAuthSnapshot,
        fallback: &Value,
    ) -> Result<String> {
        let original_fallback = if source == Source::File
            && matches!(
                self.backend,
                Backend::Codex {
                    mode: CodexMode::Auto,
                    ..
                }
            ) {
            &before.auth
        } else {
            fallback
        };
        self.projected_fingerprint(
            source,
            raw.secure_available,
            &before.auth,
            &before.metadata,
            original_fallback,
        )
    }

    fn validate_snapshot(&self, snapshot: &NativeAuthSnapshot) -> Result<()> {
        if snapshot.client != self.client {
            return Err(DaemonError::Invalid(
                "native auth snapshot client mismatch".into(),
            ));
        }
        validate_owned(&snapshot.auth, self.auth_keys())?;
        validate_owned(&snapshot.metadata, self.metadata_keys())
    }

    fn require_writable(&self, raw: &RawState) -> Result<()> {
        if matches!(
            self.backend,
            Backend::Codex {
                mode: CodexMode::Auto,
                ..
            }
        ) && !raw.secure_available
        {
            return Err(DaemonError::Locked);
        }
        Ok(())
    }

    fn check_native_lock(&self) -> Result<()> {
        let state = self.lock.lock();
        if let Some(native) = &state.borrow().native {
            native.check()?;
        }
        Ok(())
    }

    fn auth_keys(&self) -> &'static [&'static str] {
        match self.client {
            ClientKind::Codex => CODEX_KEYS,
            ClientKind::Claude => CLAUDE_KEYS,
        }
    }

    fn metadata_keys(&self) -> &'static [&'static str] {
        match self.client {
            ClientKind::Codex => &[],
            ClientKind::Claude => CLAUDE_METADATA_KEYS,
        }
    }

    fn snapshot(&self, raw: &RawState) -> Result<NativeAuthSnapshot> {
        let auth = projection(&raw.auth, self.auth_keys())?;
        let metadata = projection(&raw.metadata, self.metadata_keys())?;
        let identity = native_identity(self.client, &auth, &metadata);
        Ok(NativeAuthSnapshot {
            client: self.client,
            identity,
            auth,
            metadata,
        })
    }

    fn state_fingerprint(&self, raw: &RawState) -> Result<String> {
        let snapshot = self.snapshot(raw)?;
        let file_owned = SecretJson(projection(&raw.file_auth, self.auth_keys())?);
        self.projected_fingerprint(
            raw.source,
            raw.secure_available,
            &snapshot.auth,
            &snapshot.metadata,
            &file_owned.0,
        )
    }

    fn projected_fingerprint(
        &self,
        source: Source,
        secure_available: bool,
        auth: &Value,
        metadata: &Value,
        fallback_auth: &Value,
    ) -> Result<String> {
        #[derive(Serialize)]
        struct FingerprintState<'a> {
            store: &'a str,
            source: &'static str,
            secure_available: bool,
            auth: &'a Value,
            metadata: &'a Value,
            fallback_auth: &'a Value,
        }
        let state = FingerprintState {
            store: &self.id,
            source: source.receipt(),
            secure_available,
            auth,
            metadata,
            fallback_auth,
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&state)?);
        Ok(digest(&bytes))
    }

    fn check_backend_configuration(&self) -> Result<()> {
        if let Backend::Codex { mode, .. } = self.backend {
            let current = codex_settings(&self.config_path)?;
            if current.0 != mode {
                return Err(DaemonError::Conflict(
                    "native authentication backend changed".into(),
                ));
            }
        }
        Ok(())
    }

    fn read_state(&self) -> Result<RawState> {
        let metadata = match &self.metadata_path {
            Some(path) => read_text(path)?,
            None => Zeroizing::new(String::new()),
        };
        match &self.backend {
            Backend::ClaudeFile
            | Backend::Codex {
                mode: CodexMode::File,
                ..
            } => {
                let auth = read_text(&self.auth_path)?;
                Ok(RawState {
                    source: Source::File,
                    auth,
                    metadata,
                    file_auth: Zeroizing::new(String::new()),
                    secure_available: false,
                })
            }
            Backend::ClaudeKeychain { service, account } => {
                let auth =
                    Zeroizing::new(self.credentials.load(service, account)?.unwrap_or_default());
                Ok(RawState {
                    source: Source::Keyring,
                    auth,
                    metadata,
                    file_auth: Zeroizing::new(String::new()),
                    secure_available: true,
                })
            }
            Backend::Codex { mode, keyring } => {
                let secure = match keyring {
                    KeyringBackend::Direct => self
                        .credentials
                        .load(CODEX_AUTH_SERVICE, &codex_account(&self.home, "cli")),
                    KeyringBackend::Secrets => self.read_secrets_auth(),
                };
                let file_auth = read_text(&self.auth_path)?;
                match secure {
                    Ok(Some(value)) => Ok(RawState {
                        source: if *keyring == KeyringBackend::Direct {
                            Source::Keyring
                        } else {
                            Source::Secrets
                        },
                        auth: Zeroizing::new(value),
                        metadata,
                        file_auth,
                        secure_available: true,
                    }),
                    Ok(None) if *mode == CodexMode::Auto => Ok(RawState {
                        source: Source::File,
                        auth: file_auth.clone(),
                        metadata,
                        file_auth,
                        secure_available: true,
                    }),
                    Ok(None) => Ok(RawState {
                        source: if *keyring == KeyringBackend::Direct {
                            Source::Keyring
                        } else {
                            Source::Secrets
                        },
                        auth: Zeroizing::new(String::new()),
                        metadata,
                        file_auth,
                        secure_available: true,
                    }),
                    Err(_) if *mode == CodexMode::Auto => Ok(RawState {
                        source: Source::File,
                        auth: file_auth.clone(),
                        metadata,
                        file_auth,
                        secure_available: false,
                    }),
                    Err(error) => Err(error),
                }
            }
        }
    }

    fn read_secrets_auth(&self) -> Result<Option<String>> {
        let account = codex_account(&self.home, "secrets");
        let passphrase = self.credentials.load(CODEX_SECRETS_SERVICE, &account)?;
        let path = self.secrets_path();
        if !path.exists() {
            return Ok(None);
        }
        let passphrase = passphrase.ok_or(DaemonError::Crypto)?;
        let plaintext = decrypt_secrets(&fs::read(path)?, passphrase)?;
        let file = parse_secrets(&plaintext)?;
        match file
            .0
            .get("secrets")
            .and_then(|values| values.get(CODEX_AUTH_ENTRY))
        {
            Some(Value::String(auth)) => Ok(Some(auth.clone())),
            Some(_) => Err(native_format_error()),
            None => Ok(None),
        }
    }

    fn secrets_path(&self) -> PathBuf {
        self.home.join("secrets/codex_auth.age")
    }

    fn write_auth(&self, before: &RawState, after: &str) -> Result<()> {
        match before.source {
            Source::File => atomic_write(&self.auth_path, &before.auth, after.as_bytes()),
            Source::Keyring => {
                let (service, account) = match &self.backend {
                    Backend::ClaudeKeychain { service, account } => {
                        (service.clone(), account.clone())
                    }
                    Backend::Codex { .. } => (
                        CODEX_AUTH_SERVICE.to_owned(),
                        codex_account(&self.home, "cli"),
                    ),
                    Backend::ClaudeFile => return Err(native_format_error()),
                };
                if self
                    .credentials
                    .load(&service, &account)?
                    .unwrap_or_default()
                    != *before.auth
                {
                    return Err(native_conflict());
                }
                if after != *before.auth {
                    if object_is_empty(after)? {
                        self.credentials.delete(&service, &account)?;
                    } else {
                        self.credentials.store(&service, &account, after)?;
                    }
                }
                self.clear_codex_fallback(before)
            }
            Source::Secrets => {
                self.write_secrets_auth(before, after)?;
                self.clear_codex_fallback(before)
            }
        }
    }

    fn clear_codex_fallback(&self, before: &RawState) -> Result<()> {
        if self.client != ClientKind::Codex || before.file_auth.is_empty() {
            return Ok(());
        }
        let after = Zeroizing::new(json_patch::patch(
            &before.file_auth,
            &Value::Object(Map::new()),
            CODEX_KEYS,
        )?);
        atomic_write(&self.auth_path, &before.file_auth, after.as_bytes())
    }

    fn write_secrets_auth(&self, before: &RawState, after: &str) -> Result<()> {
        let path = self.secrets_path();
        let before_bytes = read_bytes(&path)?;
        let account = codex_account(&self.home, "secrets");
        let passphrase = match self.credentials.load(CODEX_SECRETS_SERVICE, &account)? {
            Some(value) => value,
            None if before_bytes.is_none() => {
                use rand::RngCore;
                let mut bytes = Zeroizing::new([0_u8; 32]);
                rand::rngs::OsRng.fill_bytes(bytes.as_mut());
                let value = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
                self.credentials
                    .store(CODEX_SECRETS_SERVICE, &account, &value)?;
                value
            }
            None => return Err(DaemonError::Crypto),
        };
        let passphrase = age::secrecy::SecretString::from(passphrase);
        let plaintext = match &before_bytes {
            Some(bytes) => decrypt_secrets_with_identity(bytes, &passphrase)?,
            None => Zeroizing::new(String::from("{\"version\":1,\"secrets\":{}}")),
        };
        let mut file = parse_secrets(&plaintext)?;
        let secrets = file
            .0
            .get_mut("secrets")
            .and_then(Value::as_object_mut)
            .ok_or_else(native_format_error)?;
        let current = secrets
            .get(CODEX_AUTH_ENTRY)
            .and_then(Value::as_str)
            .unwrap_or_default();
        if current != *before.auth {
            return Err(native_conflict());
        }
        if after == current {
            return Ok(());
        }
        if object_is_empty(after)? {
            secrets.remove(CODEX_AUTH_ENTRY);
        } else {
            secrets.insert(CODEX_AUTH_ENTRY.into(), Value::String(after.into()));
        }
        let output = Zeroizing::new(serde_json::to_vec(&file.0)?);
        let recipient = age::scrypt::Recipient::new(passphrase);
        let ciphertext = age::encrypt(&recipient, &output).map_err(|_| DaemonError::Crypto)?;
        atomic_write_bytes(&path, before_bytes.as_deref(), &ciphertext)
    }
}

fn codex_settings(path: &Path) -> Result<(CodexMode, KeyringBackend)> {
    let text = read_text(path)?;
    let doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| native_format_error())?;
    let mode = match doc
        .get("cli_auth_credentials_store")
        .and_then(toml_edit::Item::as_str)
    {
        None | Some("file") => CodexMode::File,
        Some("keyring") => CodexMode::Keyring,
        Some("auto") => CodexMode::Auto,
        Some("ephemeral") => {
            return Err(DaemonError::Invalid(
                "ephemeral native authentication cannot be managed".into(),
            ));
        }
        Some(_) => return Err(native_format_error()),
    };
    // Codex selects this backend by platform; it has no public TOML override.
    let keyring = if cfg!(windows) {
        KeyringBackend::Secrets
    } else {
        KeyringBackend::Direct
    };
    Ok((mode, keyring))
}

fn claude_metadata_path(home: &Path, explicit: bool) -> Result<PathBuf> {
    let legacy = home.join(".config.json");
    if legacy.exists() {
        return Ok(legacy);
    }
    Ok(if explicit {
        home.join(".claude.json")
    } else {
        user_home()?.join(".claude.json")
    })
}

fn claude_service(home: &Path, explicit: bool) -> String {
    let suffix = if explicit {
        let normalized: String = home.to_string_lossy().nfc().collect();
        format!("-{}", &digest(normalized.as_bytes())[..8])
    } else {
        String::new()
    };
    format!("Claude Code-credentials{suffix}")
}

fn claude_account() -> String {
    let account = std::env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(platform_username)
        .unwrap_or_default();
    if !account.is_empty()
        && account
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        account
    } else {
        "claude-code-user".into()
    }
}

fn platform_username() -> Option<String> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .ok()?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .ok()
                .map(|value| value.trim().to_owned());
        }
        None
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").ok()
    }
}

fn user_home() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| DaemonError::Config("cannot resolve native client home".into()))
}

fn canonical_path(path: &Path) -> String {
    if let Ok(canonical) = path.canonicalize() {
        return canonical.to_string_lossy().into_owned();
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canonical) = parent.canonicalize()
    {
        return canonical.join(name).to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

fn codex_account(home: &Path, prefix: &str) -> String {
    format!(
        "{prefix}|{}",
        &digest(canonical_path(home).as_bytes())[..16]
    )
}

fn parse_object(text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value = jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
        .map_err(|_| native_format_error())?
        .ok_or_else(native_format_error)?;
    match value {
        Value::Object(object) => Ok(object),
        mut value => {
            wipe_json(&mut value);
            Err(native_format_error())
        }
    }
}

fn validate_object(text: &str) -> Result<()> {
    let mut object = parse_object(text)?;
    object.values_mut().for_each(wipe_json);
    Ok(())
}

fn object_is_empty(text: &str) -> Result<bool> {
    let mut object = parse_object(text)?;
    let empty = object.is_empty();
    object.values_mut().for_each(wipe_json);
    Ok(empty)
}

fn projection(text: &str, keys: &[&str]) -> Result<Value> {
    let mut object = parse_object(text)?;
    let mut owned = Map::new();
    for key in keys {
        if let Some(value) = object.remove(*key) {
            owned.insert((*key).into(), value);
        }
    }
    object.values_mut().for_each(wipe_json);
    Ok(Value::Object(owned))
}

fn validate_owned(value: &Value, keys: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or_else(native_format_error)?;
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(DaemonError::Invalid(
            "native auth snapshot includes unowned fields".into(),
        ));
    }
    Ok(())
}

fn native_identity(client: ClientKind, auth: &Value, metadata: &Value) -> Option<NativeIdentity> {
    match client {
        ClientKind::Codex => {
            let tokens = auth.get("tokens")?.as_object()?;
            for key in ["access_token", "refresh_token"] {
                if tokens
                    .get(key)
                    .and_then(Value::as_str)
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return None;
                }
            }
            let jwt = tokens.get("id_token")?.as_str()?;
            let claims = SecretJson(jwt_claims(jwt)?);
            let namespace = claims.0.get("https://api.openai.com/auth");
            let account_id = namespace
                .and_then(|value| {
                    nonempty(value, "chatgpt_user_id").or_else(|| nonempty(value, "user_id"))
                })
                .or_else(|| nonempty(&claims.0, "sub"))?;
            let organization_id = tokens
                .get("account_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| namespace.and_then(|value| nonempty(value, "chatgpt_account_id")))?;
            let email = nonempty(&claims.0, "email").or_else(|| {
                claims
                    .0
                    .get("https://api.openai.com/profile")
                    .and_then(|value| nonempty(value, "email"))
            });
            let native_name = nonempty(&claims.0, "name");
            Some(NativeIdentity {
                account_id,
                organization_id,
                email,
                native_name,
            })
        }
        ClientKind::Claude => {
            let oauth = auth.get("claudeAiOauth")?.as_object()?;
            if oauth
                .get("accessToken")
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
                || !oauth
                    .get("scopes")
                    .and_then(Value::as_array)
                    .is_some_and(|scopes| {
                        scopes
                            .iter()
                            .any(|scope| scope.as_str() == Some("user:inference"))
                    })
            {
                return None;
            }
            let account = metadata.get("oauthAccount")?;
            Some(NativeIdentity {
                account_id: nonempty(account, "accountUuid")?,
                organization_id: nonempty(account, "organizationUuid")?,
                email: nonempty(account, "emailAddress"),
                native_name: nonempty(account, "displayName")
                    .or_else(|| nonempty(account, "fullName")),
            })
        }
    }
}

fn nonempty(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn jwt_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if header.is_empty() || signature.is_empty() || parts.next().is_some() {
        return None;
    }
    let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).ok()?);
    serde_json::from_slice(&bytes).ok()
}

fn parse_secrets(text: &str) -> Result<SecretJson> {
    let value = SecretJson(serde_json::from_str(text).map_err(|_| native_format_error())?);
    if !matches!(value.0.get("version").and_then(Value::as_u64), Some(0 | 1))
        || !value.0.get("secrets").is_some_and(Value::is_object)
    {
        return Err(native_format_error());
    }
    Ok(value)
}

fn decrypt_secrets(bytes: &[u8], passphrase: String) -> Result<Zeroizing<String>> {
    decrypt_secrets_with_identity(bytes, &age::secrecy::SecretString::from(passphrase))
}

fn decrypt_secrets_with_identity(
    bytes: &[u8],
    passphrase: &age::secrecy::SecretString,
) -> Result<Zeroizing<String>> {
    let identity = age::scrypt::Identity::new(passphrase.clone());
    let plaintext =
        Zeroizing::new(age::decrypt(&identity, bytes).map_err(|_| DaemonError::Crypto)?);
    std::str::from_utf8(&plaintext)
        .map(|text| Zeroizing::new(text.to_owned()))
        .map_err(|_| native_format_error())
}

fn read_text(path: &Path) -> Result<Zeroizing<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Zeroizing::new(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Zeroizing::new(String::new()))
        }
        Err(error) => Err(DaemonError::Io(error)),
    }
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DaemonError::Io(error)),
    }
}

fn atomic_write(path: &Path, before: &str, after: &[u8]) -> Result<()> {
    let expected = if path.exists() {
        Some(before.as_bytes())
    } else {
        None
    };
    atomic_write_bytes(path, expected, after)
}

fn atomic_write_bytes(path: &Path, before: Option<&[u8]>, after: &[u8]) -> Result<()> {
    if read_bytes(path)?.as_deref() != before {
        return Err(native_conflict());
    }
    if before == Some(after) || (before.is_none() && after.is_empty()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = AtomicWriteFile::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(path).map_or_else(
            |_| fs::Permissions::from_mode(0o600),
            |metadata| metadata.permissions(),
        );
        output.set_permissions(permissions)?;
    }
    output.write_all(after)?;
    if read_bytes(path)?.as_deref() != before {
        return Err(native_conflict());
    }
    output.commit()?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn native_conflict() -> DaemonError {
    DaemonError::Conflict("native authentication changed outside hsin".into())
}

fn native_format_error() -> DaemonError {
    DaemonError::Config("native authentication storage has an unsupported format".into())
}

/// Delete only credentials belonging to an isolated OAuth login directory.
pub(crate) fn cleanup_login_home_with_credentials(
    client: ClientKind,
    home: &Path,
    credentials: &dyn NativeCredentialStore,
) -> Result<()> {
    let marker_path = home.join(".hsin-official-login");
    let marker_is_regular = fs::symlink_metadata(&marker_path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
    let home_is_regular = fs::symlink_metadata(home)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
    if !marker_is_regular || !home_is_regular {
        return Err(DaemonError::Invalid(
            "refusing to clean a non-login native auth directory".into(),
        ));
    }
    let marker: Value = serde_json::from_slice(&fs::read(marker_path)?)
        .map_err(|_| DaemonError::Invalid("invalid isolated native login marker".into()))?;
    let marker_client = marker.get("client").and_then(Value::as_str);
    let expected_client = match client {
        ClientKind::Codex => "codex",
        ClientKind::Claude => "claude",
    };
    if marker_client != Some(expected_client)
        || marker
            .get("login_id")
            .and_then(Value::as_str)
            .is_none_or(|value| uuid::Uuid::parse_str(value).is_err())
        || canonical_path(home) == canonical_path(&user_home()?)
        || canonical_path(home) == canonical_path(&user_home()?.join(".codex"))
        || canonical_path(home) == canonical_path(&user_home()?.join(".claude"))
    {
        return Err(DaemonError::Invalid(
            "refusing to clean a non-login native auth directory".into(),
        ));
    }
    match client {
        ClientKind::Codex => {
            credentials.delete(CODEX_AUTH_SERVICE, &codex_account(home, "cli"))?;
            if !["local.age", "mcp_oauth.age", "gateway_oauth.age"]
                .iter()
                .any(|name| home.join("secrets").join(name).exists())
            {
                credentials.delete(CODEX_SECRETS_SERVICE, &codex_account(home, "secrets"))?;
            }
        }
        ClientKind::Claude if cfg!(target_os = "macos") => {
            credentials.delete(&claude_service(home, true), &claude_account())?;
        }
        ClientKind::Claude => {}
    }
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryNativeCredentialStore {
    values: parking_lot::Mutex<std::collections::BTreeMap<(String, String), Zeroizing<String>>>,
}

#[cfg(test)]
impl NativeCredentialStore for MemoryNativeCredentialStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>> {
        Ok(self
            .values
            .lock()
            .get(&(service.into(), account.into()))
            .map(|value| value.to_string()))
    }
    fn store(&self, service: &str, account: &str, value: &str) -> Result<()> {
        self.values.lock().insert(
            (service.into(), account.into()),
            Zeroizing::new(value.into()),
        );
        Ok(())
    }
    fn delete(&self, service: &str, account: &str) -> Result<()> {
        self.values.lock().remove(&(service.into(), account.into()));
        Ok(())
    }
}

#[cfg(test)]
#[path = "native_auth/tests.rs"]
mod tests;
