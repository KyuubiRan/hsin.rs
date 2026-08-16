use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use keyring::Entry;
use parking_lot::RwLock;
use rand::{RngCore, rngs::OsRng};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    db::{Database, EncryptedProtectedValue, EncryptedSecret},
    error::{DaemonError, Result},
    model::{ClientKind, Provider},
};

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;
const VERIFIER_TEXT: &[u8] = b"hsin master key verifier v1";

#[derive(Zeroize)]
#[zeroize(drop)]
struct KeyMaterial {
    version: u32,
    key: [u8; KEY_BYTES],
}

pub trait KeyStore: Send + Sync {
    fn load(&self, version: u32) -> Result<Option<String>>;
    fn store(&self, version: u32, value: &str) -> Result<()>;
    fn delete(&self, version: u32) -> Result<()>;
}

pub struct SystemKeyStore {
    account_scope: String,
}

impl SystemKeyStore {
    pub fn for_home(home: &Path) -> Self {
        let normalized = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        let digest = Sha256::digest(normalized.as_os_str().as_encoded_bytes());
        Self {
            account_scope: hex::encode(&digest[..8]),
        }
    }

    fn entry(&self, version: u32) -> Result<Entry> {
        Entry::new(
            "dev.hsin.master-key",
            &format!("home-{}-master-key-v{version}", self.account_scope),
        )
        .map_err(|error| DaemonError::Keyring(error.to_string()))
    }
}

impl KeyStore for SystemKeyStore {
    fn load(&self, version: u32) -> Result<Option<String>> {
        match self.entry(version)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(DaemonError::Keyring(error.to_string())),
        }
    }

    fn store(&self, version: u32, value: &str) -> Result<()> {
        self.entry(version)?
            .set_password(value)
            .map_err(|error| DaemonError::Keyring(error.to_string()))
    }

    fn delete(&self, version: u32) -> Result<()> {
        match self.entry(version)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(DaemonError::Keyring(error.to_string())),
        }
    }
}

/// Master keys handed to the daemon by systemd through `LoadCredentialEncrypted=`.
///
/// A system unit has no session bus, so the Secret Service that backs
/// [`SystemKeyStore`] is unreachable there. systemd decrypts the sealed key into
/// `$CREDENTIALS_DIRECTORY`, a read-only tmpfs private to the service, which is
/// why every write here fails loudly instead of silently landing somewhere the
/// next start would not read back.
pub struct CredentialKeyStore {
    directory: PathBuf,
}

impl CredentialKeyStore {
    /// systemd exports this only for units that declare credentials, so its
    /// presence is what selects this store over the Secret Service one.
    pub fn from_environment() -> Option<Self> {
        std::env::var_os("CREDENTIALS_DIRECTORY").map(|directory| Self {
            directory: PathBuf::from(directory),
        })
    }

    #[must_use]
    pub fn credential_name(version: u32) -> String {
        format!("hsin-master-key-v{version}")
    }

    fn read_only(version: u32) -> DaemonError {
        DaemonError::Keyring(format!(
            "systemd credential {} is read-only; re-provision it with `sudo hsind service install --system`",
            Self::credential_name(version)
        ))
    }
}

impl KeyStore for CredentialKeyStore {
    fn load(&self, version: u32) -> Result<Option<String>> {
        let path = self.directory.join(Self::credential_name(version));
        match std::fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim();
                Ok((!value.is_empty()).then(|| value.to_owned()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DaemonError::Keyring(error.to_string())),
        }
    }

    /// Provisioning happens out of band, so the only accepted write is the
    /// no-op one: re-storing the key systemd already supplied. Importing a
    /// recovery key that matches the sealed credential must succeed.
    fn store(&self, version: u32, value: &str) -> Result<()> {
        if self.load(version)?.as_deref() == Some(value) {
            return Ok(());
        }
        Err(Self::read_only(version))
    }

    fn delete(&self, version: u32) -> Result<()> {
        if self.load(version)?.is_none() {
            return Ok(());
        }
        Err(Self::read_only(version))
    }
}

/// Pick the key store for the current process: systemd credentials when the
/// daemon runs as a system unit, the OS keyring otherwise.
pub fn default_key_store(home: &Path) -> Arc<dyn KeyStore> {
    CredentialKeyStore::from_environment().map_or_else(
        || Arc::new(SystemKeyStore::for_home(home)) as Arc<dyn KeyStore>,
        |store| Arc::new(store) as Arc<dyn KeyStore>,
    )
}

pub struct CryptoManager {
    db: Arc<Database>,
    store: Arc<dyn KeyStore>,
    material: RwLock<Option<KeyMaterial>>,
    lock_reason: RwLock<Option<String>>,
}

impl CryptoManager {
    pub fn initialize(db: Arc<Database>, store: Arc<dyn KeyStore>) -> Result<Self> {
        let manager = Self {
            db,
            store,
            material: RwLock::new(None),
            lock_reason: RwLock::new(None),
        };
        if let Some((version, nonce, verifier)) = manager.db.current_key_record()? {
            match manager.store.load(version) {
                Ok(Some(encoded)) => match decode_key(&encoded)
                    .and_then(|key| verify_key(&key, version, &nonce, &verifier).map(|()| key))
                {
                    Ok(key) => *manager.material.write() = Some(KeyMaterial { version, key }),
                    Err(error) => {
                        let reason = format!("stored master key failed verification: {error}");
                        tracing::error!(%reason, "daemon is locked; import the recovery key");
                        *manager.lock_reason.write() = Some(reason);
                    }
                },
                Ok(None) => {
                    let reason = "master key is missing from the system keyring".to_string();
                    tracing::error!(%reason, "daemon is locked; import the recovery key");
                    *manager.lock_reason.write() = Some(reason);
                }
                Err(error) => {
                    tracing::warn!(%error,"system keyring unavailable; daemon is locked");
                    *manager.lock_reason.write() = Some(error.to_string());
                }
            }
        } else {
            let version = 1;
            // A provisioned store already holds the key for an empty database:
            // `hsind service install --system` seals it before the daemon first
            // runs. Generating a second key here would strand that credential.
            let key = match manager.store.load(version) {
                Ok(Some(encoded)) => match decode_key(&encoded) {
                    Ok(key) => key,
                    Err(error) => {
                        let reason = format!("provisioned master key is unusable: {error}");
                        tracing::error!(%reason, "daemon is locked");
                        *manager.lock_reason.write() = Some(reason);
                        return Ok(manager);
                    }
                },
                Ok(None) => {
                    let key = random_key();
                    let encoded = URL_SAFE_NO_PAD.encode(key);
                    if let Err(error) = manager.store.store(version, &encoded) {
                        tracing::warn!(%error,"system keyring unavailable; daemon is locked");
                        *manager.lock_reason.write() = Some(error.to_string());
                        return Ok(manager);
                    }
                    key
                }
                Err(error) => {
                    tracing::warn!(%error,"system keyring unavailable; daemon is locked");
                    *manager.lock_reason.write() = Some(error.to_string());
                    return Ok(manager);
                }
            };
            let (nonce, verifier) = make_verifier(&key, version)?;
            manager
                .db
                .initialize_key_record(version, &nonce, &verifier)?;
            *manager.material.write() = Some(KeyMaterial { version, key });
        }
        Ok(manager)
    }

    pub fn is_unlocked(&self) -> bool {
        self.material.read().is_some()
    }

    pub fn lock_reason(&self) -> Option<String> {
        self.lock_reason.read().clone()
    }

    pub fn encrypt_for(&self, provider: &Provider, value: &str) -> Result<EncryptedSecret> {
        let material = self.material.read();
        let material = material.as_ref().ok_or(DaemonError::Locked)?;
        encrypt_secret(provider, value.as_bytes(), material)
    }

    pub fn decrypt_for(
        &self,
        provider: &Provider,
        encrypted: &EncryptedSecret,
    ) -> Result<SecretString> {
        let material = self.material.read();
        let material = material.as_ref().ok_or(DaemonError::Locked)?;
        if encrypted.key_version != material.version {
            return Err(DaemonError::Crypto);
        }
        let plaintext = decrypt_secret(provider, encrypted, material)?;
        let text = String::from_utf8(plaintext.to_vec()).map_err(|_| DaemonError::Crypto)?;
        Ok(SecretString::from(text))
    }

    pub fn encrypt_protected(&self, key: &str, value: &[u8]) -> Result<EncryptedProtectedValue> {
        let material = self.material.read();
        let material = material.as_ref().ok_or(DaemonError::Locked)?;
        encrypt_protected_value(key, value, material)
    }

    pub fn decrypt_protected(
        &self,
        encrypted: &EncryptedProtectedValue,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let material = self.material.read();
        let material = material.as_ref().ok_or(DaemonError::Locked)?;
        decrypt_protected(encrypted, material)
    }

    pub fn export_recovery_key(&self) -> Result<SecretString> {
        let material = self.material.read();
        let material = material.as_ref().ok_or(DaemonError::Locked)?;
        Ok(SecretString::from(format!(
            "hsin-recovery-v1:{}:{}",
            material.version,
            URL_SAFE_NO_PAD.encode(material.key)
        )))
    }

    pub fn import_recovery_key(&self, recovery: &str) -> Result<()> {
        let (version, key) = parse_recovery(recovery)?;
        let (current_version, nonce, verifier) =
            self.db.current_key_record()?.ok_or(DaemonError::Crypto)?;
        if version != current_version {
            return Err(DaemonError::Invalid(
                "recovery key version does not match the database".into(),
            ));
        }
        verify_key(&key, version, &nonce, &verifier)?;
        self.store.store(version, &URL_SAFE_NO_PAD.encode(key))?;
        *self.material.write() = Some(KeyMaterial { version, key });
        *self.lock_reason.write() = None;
        Ok(())
    }

    pub fn rotate(&self) -> Result<u32> {
        self.retry_pending_cleanup()?;
        let providers = self.db.list_providers(None)?;
        let (old_version, old_key) = {
            let guard = self.material.read();
            let material = guard.as_ref().ok_or(DaemonError::Locked)?;
            (material.version, material.key)
        };
        let old_material = KeyMaterial {
            version: old_version,
            key: old_key,
        };
        let new_material = KeyMaterial {
            version: old_material
                .version
                .checked_add(1)
                .ok_or(DaemonError::Crypto)?,
            key: random_key(),
        };
        let mut rotated = Vec::new();
        for encrypted in self.db.all_secrets()? {
            let provider = providers
                .iter()
                .find(|provider| provider.id == encrypted.provider_id)
                .ok_or_else(|| DaemonError::NotFound("provider for credential".into()))?;
            let plaintext = decrypt_secret(provider, &encrypted, &old_material)?;
            rotated.push(encrypt_secret(provider, &plaintext, &new_material)?);
        }
        let mut rotated_protected = Vec::new();
        for encrypted in self.db.all_protected_values()? {
            let plaintext = decrypt_protected(&encrypted, &old_material)?;
            rotated_protected.push(encrypt_protected_value(
                &encrypted.key,
                &plaintext,
                &new_material,
            )?);
        }
        self.store.store(
            new_material.version,
            &URL_SAFE_NO_PAD.encode(new_material.key),
        )?;
        let (nonce, verifier) = make_verifier(&new_material.key, new_material.version)?;
        if let Err(error) = self.db.replace_secrets_and_key(
            &rotated,
            &rotated_protected,
            old_version,
            new_material.version,
            &nonce,
            &verifier,
        ) {
            let _ = self.store.delete(new_material.version);
            return Err(error);
        }
        let new_version = new_material.version;
        *self.material.write() = Some(new_material);
        if let Err(error) = self.store.delete(old_version) {
            return Err(DaemonError::Keyring(format!(
                "key rotation completed, but old key version {old_version} could not be removed: {error}"
            )));
        }
        self.db.set_setting("key_cleanup_pending", "")?;
        Ok(new_version)
    }

    pub fn retry_pending_cleanup(&self) -> Result<Option<u32>> {
        let Some(value) = self.db.setting("key_cleanup_pending")? else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        let version = value
            .parse::<u32>()
            .map_err(|_| DaemonError::Internal("invalid key cleanup state".into()))?;
        self.store.delete(version)?;
        self.db.set_setting("key_cleanup_pending", "")?;
        Ok(Some(version))
    }

    pub fn credential_for(&self, client: ClientKind) -> Result<SecretString> {
        let (provider, encrypted) = self.db.active_secret(client)?;
        self.decrypt_for(&provider, &encrypted)
    }
}

fn encrypt_secret(
    provider: &Provider,
    plaintext: &[u8],
    material: &KeyMaterial,
) -> Result<EncryptedSecret> {
    let mut nonce = [0_u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new((&material.key).into());
    let aad = aad(provider, material.version);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| DaemonError::Crypto)?;
    Ok(EncryptedSecret {
        provider_id: provider.id.clone(),
        key_version: material.version,
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

fn decrypt_secret(
    provider: &Provider,
    encrypted: &EncryptedSecret,
    material: &KeyMaterial,
) -> Result<Zeroizing<Vec<u8>>> {
    if encrypted.nonce.len() != NONCE_BYTES {
        return Err(DaemonError::Crypto);
    }
    let cipher = XChaCha20Poly1305::new((&material.key).into());
    let aad = aad(provider, encrypted.key_version);
    cipher
        .decrypt(
            XNonce::from_slice(&encrypted.nonce),
            Payload {
                msg: &encrypted.ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| DaemonError::Crypto)
}

fn encrypt_protected_value(
    key: &str,
    plaintext: &[u8],
    material: &KeyMaterial,
) -> Result<EncryptedProtectedValue> {
    let mut nonce = [0_u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new((&material.key).into());
    let aad = protected_aad(key, material.version);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| DaemonError::Crypto)?;
    Ok(EncryptedProtectedValue {
        key: key.to_owned(),
        key_version: material.version,
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

fn decrypt_protected(
    encrypted: &EncryptedProtectedValue,
    material: &KeyMaterial,
) -> Result<Zeroizing<Vec<u8>>> {
    if encrypted.key_version != material.version || encrypted.nonce.len() != NONCE_BYTES {
        return Err(DaemonError::Crypto);
    }
    let cipher = XChaCha20Poly1305::new((&material.key).into());
    let aad = protected_aad(&encrypted.key, encrypted.key_version);
    cipher
        .decrypt(
            XNonce::from_slice(&encrypted.nonce),
            Payload {
                msg: &encrypted.ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| DaemonError::Crypto)
}

fn aad(provider: &Provider, version: u32) -> String {
    format!("hsin:v1:{}:{}:{version}", provider.client, provider.id)
}

fn protected_aad(key: &str, version: u32) -> String {
    format!("hsin:v1:protected:{key}:{version}")
}

fn make_verifier(key: &[u8; KEY_BYTES], version: u32) -> Result<(Vec<u8>, Vec<u8>)> {
    let material = KeyMaterial { version, key: *key };
    let provider = Provider {
        id: "master-key".into(),
        client: ClientKind::Codex,
        name: String::new(),
        description: String::new(),
        base_url: String::new(),
        auth_scheme: crate::model::AuthScheme::Bearer,
        official: false,
        credential_configured: false,
        credential_preview: None,
        model: None,
        codex_config_name: None,
        claude_model_mapping: None,
        revision: 0,
    };
    let encrypted = encrypt_secret(&provider, VERIFIER_TEXT, &material)?;
    Ok((encrypted.nonce, encrypted.ciphertext))
}

fn verify_key(key: &[u8; KEY_BYTES], version: u32, nonce: &[u8], verifier: &[u8]) -> Result<()> {
    let material = KeyMaterial { version, key: *key };
    let provider = Provider {
        id: "master-key".into(),
        client: ClientKind::Codex,
        name: String::new(),
        description: String::new(),
        base_url: String::new(),
        auth_scheme: crate::model::AuthScheme::Bearer,
        official: false,
        credential_configured: false,
        credential_preview: None,
        model: None,
        codex_config_name: None,
        claude_model_mapping: None,
        revision: 0,
    };
    let encrypted = EncryptedSecret {
        provider_id: provider.id.clone(),
        key_version: version,
        nonce: nonce.to_vec(),
        ciphertext: verifier.to_vec(),
    };
    let value = decrypt_secret(&provider, &encrypted, &material)?;
    if value.as_slice().ct_eq(VERIFIER_TEXT).into() {
        Ok(())
    } else {
        Err(DaemonError::Crypto)
    }
}

fn random_key() -> [u8; KEY_BYTES] {
    let mut key = [0_u8; KEY_BYTES];
    OsRng.fill_bytes(&mut key);
    key
}
fn decode_key(encoded: &str) -> Result<[u8; KEY_BYTES]> {
    let mut decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| DaemonError::Crypto)?;
    if decoded.len() != KEY_BYTES {
        decoded.zeroize();
        return Err(DaemonError::Crypto);
    }
    let mut key = [0_u8; KEY_BYTES];
    key.copy_from_slice(&decoded);
    decoded.zeroize();
    Ok(key)
}
/// Decode a recovery key into the key-store encoding, so provisioning can seal
/// an existing master key without the database or a running daemon.
#[cfg(target_os = "linux")]
pub fn recovery_key_material(recovery: &str) -> Result<(u32, Zeroizing<String>)> {
    let (version, mut key) = parse_recovery(recovery)?;
    let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(key));
    key.zeroize();
    Ok((version, encoded))
}

fn parse_recovery(value: &str) -> Result<(u32, [u8; KEY_BYTES])> {
    let mut parts = value.trim().split(':');
    if parts.next() != Some("hsin-recovery-v1") {
        return Err(DaemonError::Invalid("invalid recovery key format".into()));
    }
    let version = parts
        .next()
        .ok_or_else(|| DaemonError::Invalid("missing recovery key version".into()))?
        .parse()
        .map_err(|_| DaemonError::Invalid("invalid recovery key version".into()))?;
    let key = decode_key(
        parts
            .next()
            .ok_or_else(|| DaemonError::Invalid("missing recovery key data".into()))?,
    )?;
    if parts.next().is_some() {
        return Err(DaemonError::Invalid("invalid recovery key format".into()));
    }
    Ok((version, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthScheme, ProviderInput};
    use parking_lot::Mutex;
    use secrecy::ExposeSecret;
    use std::{collections::HashMap, fs};

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<u32, String>>);
    impl KeyStore for MemoryStore {
        fn load(&self, version: u32) -> Result<Option<String>> {
            Ok(self.0.lock().get(&version).cloned())
        }
        fn store(&self, version: u32, value: &str) -> Result<()> {
            self.0.lock().insert(version, value.into());
            Ok(())
        }
        fn delete(&self, version: u32) -> Result<()> {
            self.0.lock().remove(&version);
            Ok(())
        }
    }

    #[test]
    fn system_key_store_is_scoped_to_the_data_home() {
        let first = SystemKeyStore::for_home(Path::new("/tmp/hsin-one"));
        let same = SystemKeyStore::for_home(Path::new("/tmp/hsin-one"));
        let second = SystemKeyStore::for_home(Path::new("/tmp/hsin-two"));
        assert_eq!(first.account_scope, same.account_scope);
        assert_ne!(first.account_scope, second.account_scope);
    }

    /// `$CREDENTIALS_DIRECTORY` is a read-only tmpfs. A write that silently
    /// succeeded would vanish on restart and leave the database permanently
    /// locked, so every write that would change the sealed key must fail here.
    #[test]
    fn systemd_credentials_reject_writes_that_would_not_survive_a_restart() {
        let root = std::env::temp_dir().join(format!("hsind-cred-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = CredentialKeyStore {
            directory: root.clone(),
        };
        assert_eq!(store.load(1).unwrap(), None);
        // Deleting a credential that was never provisioned is a no-op, so
        // rotation cleanup on a fresh install must not report a failure.
        store.delete(1).unwrap();

        fs::write(
            root.join(CredentialKeyStore::credential_name(1)),
            "sealed\n",
        )
        .unwrap();
        assert_eq!(store.load(1).unwrap().as_deref(), Some("sealed"));
        // Re-storing the key systemd already supplied is the one accepted
        // write: importing a matching recovery key must not fail.
        store.store(1, "sealed").unwrap();
        assert!(store.store(1, "different").is_err());
        assert!(store.delete(1).is_err());
    }

    /// `hsind service install --system` seals the master key before the daemon
    /// ever runs, so the first start finds a provisioned store and an empty
    /// database. Generating a fresh key there would strand the sealed one and
    /// lock the daemon on the next restart.
    #[test]
    fn initialization_adopts_a_provisioned_key_instead_of_generating_one() {
        let root = std::env::temp_dir().join(format!("hsind-provision-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let provisioned = URL_SAFE_NO_PAD.encode([7_u8; KEY_BYTES]);
        let store = Arc::new(MemoryStore::default());
        store.store(1, &provisioned).unwrap();

        let db = Arc::new(Database::open(&root.join("db"), &root.join("backups")).unwrap());
        let crypto = CryptoManager::initialize(db, store.clone()).unwrap();

        assert!(crypto.is_unlocked());
        assert_eq!(
            crypto.export_recovery_key().unwrap().expose_secret(),
            format!("hsin-recovery-v1:1:{provisioned}")
        );
        assert_eq!(
            store.load(1).unwrap().as_deref(),
            Some(provisioned.as_str())
        );
    }

    #[test]
    fn encrypt_tamper_export_and_rotate() {
        let root = std::env::temp_dir().join(format!("hsind-crypto-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let db = Arc::new(Database::open(&root.join("db"), &root.join("backups")).unwrap());
        let crypto =
            CryptoManager::initialize(db.clone(), Arc::new(MemoryStore::default())).unwrap();
        let provider = db
            .add_provider(&ProviderInput {
                client: ClientKind::Codex,
                name: "x".into(),
                description: String::new(),
                base_url: "https://example.test".into(),
                auth_scheme: AuthScheme::Bearer,
                model: None,
                codex_config_name: None,
                claude_model_mapping: None,
            })
            .unwrap();
        let encrypted = crypto.encrypt_for(&provider, "secret").unwrap();
        db.put_secret(&encrypted).unwrap();
        let protected = crypto
            .encrypt_protected("codex_auth_backup_v1", b"protected-secret")
            .unwrap();
        db.put_protected_value(&protected).unwrap();
        db.set_active(ClientKind::Codex, &provider.id, "managed")
            .unwrap();
        assert_eq!(
            crypto
                .credential_for(ClientKind::Codex)
                .unwrap()
                .expose_secret(),
            "secret"
        );
        let recovery = crypto.export_recovery_key().unwrap();
        assert!(recovery.expose_secret().starts_with("hsin-recovery-v1:"));
        assert_eq!(crypto.rotate().unwrap(), 2);
        assert_eq!(
            crypto
                .credential_for(ClientKind::Codex)
                .unwrap()
                .expose_secret(),
            "secret"
        );
        let protected = db.protected_value("codex_auth_backup_v1").unwrap().unwrap();
        assert_eq!(
            crypto.decrypt_protected(&protected).unwrap().as_slice(),
            b"protected-secret"
        );
        let mut bad_protected = protected;
        bad_protected.ciphertext[0] ^= 1;
        assert!(crypto.decrypt_protected(&bad_protected).is_err());
        let mut bad = db.secret(&provider.id).unwrap();
        bad.ciphertext[0] ^= 1;
        assert!(crypto.decrypt_for(&provider, &bad).is_err());
        drop(crypto);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }
}
