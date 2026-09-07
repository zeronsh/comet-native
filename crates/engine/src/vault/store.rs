//! Local persistence for one profile's vault membership: this device's
//! private identity, the pinned membership history, the keyring, and cached
//! object keys (RFC 0001 §5, §13; plan ES-04, G5).
//!
//! The state file lives under the profile store root and is encrypted with
//! AES-256-GCM under a 32-byte *protection key* the platform supplies:
//!
//! * macOS — a login-Keychain generic password (device-bound; not synced).
//! * Linux/headless — an operator-provisioned credential: a systemd
//!   `$CREDENTIALS_DIRECTORY` entry or an explicit `ZERON_VAULT_KEY_FILE`
//!   (0600). Both are the RFC's "unattended" mode and are opt-in by their
//!   presence; without one the vault stays LOCKED. There is no silent
//!   plaintext fallback.
//! * tests — an in-memory provider.
//!
//! Failure to obtain the protection key never erases the encrypted file; a
//! store that cannot be opened reports `Locked` and keeps the bytes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use zeron_crypto::{CryptoError, SecretBytes};

use crate::EngineError;

const STATE_FILE: &str = "vault.json";
const FILE_VERSION: u32 = 1;
const FILE_AAD: &[u8] = b"zeron/vault-state-file/v1\0";
const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;

/// Where the protection key came from; reported in status so the user
/// knows which credential mode protects this device's keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtectionMode {
    Keychain,
    SystemdCredential,
    KeyFile,
    Memory,
}

/// Supplies the device-held protection key. `load_or_create` must be
/// idempotent and must never return a fresh key while one already exists
/// (that would silently orphan the encrypted state).
pub trait ProtectionKeyProvider: Send + Sync + 'static {
    fn mode(&self) -> ProtectionMode;
    fn load_or_create(&self, profile_key: &str) -> Result<SecretBytes, VaultStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum VaultStoreError {
    #[error("secure key storage is unavailable: {0}")]
    Locked(String),
    #[error("vault state is corrupt: {0}")]
    Corrupt(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("crypto: {0}")]
    Crypto(CryptoError),
}

impl From<CryptoError> for VaultStoreError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

impl From<VaultStoreError> for EngineError {
    fn from(error: VaultStoreError) -> Self {
        EngineError::Other(error.to_string())
    }
}

/// Hex-encoded byte fields keep the JSON readable in diagnostics without
/// ever printing secrets (secret fields are wrapped in [`Secret`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hex(pub String);

impl Hex {
    pub fn of(bytes: &[u8]) -> Self {
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn decode<const N: usize>(&self) -> Option<[u8; N]> {
        let bytes = self.bytes()?;
        bytes.try_into().ok()
    }

    pub fn bytes(&self) -> Option<Vec<u8>> {
        if !self.0.len().is_multiple_of(2) {
            return None;
        }
        (0..self.0.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&self.0[i..i + 2], 16).ok())
            .collect()
    }
}

/// A secret byte field: serializes as hex but never appears in Debug.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub Hex);

impl Secret {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Hex::of(bytes))
    }
    pub fn decode<const N: usize>(&self) -> Option<[u8; N]> {
        self.0.decode()
    }
    pub fn bytes(&self) -> Option<Vec<u8>> {
        self.0.bytes()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub device_id: Hex,
    pub signing_seed: Secret,
    pub encryption_secret: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinnedVault {
    pub vault_id: Hex,
    pub generation: Hex,
    pub profile_hash: Hex,
    /// Every verified policy record, base64, genesis first.
    pub membership: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedObjectKey {
    pub epoch: u64,
    pub key_id: Hex,
    pub key: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEnrollment {
    pub request_id: Hex,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalVaultState {
    pub version: u32,
    pub device: Option<DeviceIdentity>,
    pub vault: Option<PinnedVault>,
    /// Encoded keyring (`zeron_crypto::keyring::Keyring::encode`), hex.
    pub keyring: Option<Secret>,
    /// Object id (hex) → wrapped keys this device has unwrapped.
    #[serde(default)]
    pub object_keys: std::collections::BTreeMap<String, Vec<CachedObjectKey>>,
    pub enrollment: Option<PendingEnrollment>,
    /// Envelopes this device still owes after a rotation it authored
    /// (recipient id hex, epoch) — retried on every refresh until published.
    #[serde(default)]
    pub owed_envelopes: Vec<(Hex, u64)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    version: u32,
    protection: ProtectionMode,
    nonce: Hex,
    ciphertext: Hex,
}

/// The encrypted state file plus its protection key.
pub struct VaultStore {
    path: PathBuf,
    profile_key: String,
    provider: Box<dyn ProtectionKeyProvider>,
    protection_key: Mutex<Option<SecretBytes>>,
}

impl VaultStore {
    pub fn new(
        store_root: &Path,
        profile_key: impl Into<String>,
        provider: Box<dyn ProtectionKeyProvider>,
    ) -> Self {
        Self {
            path: store_root.join(STATE_FILE),
            profile_key: profile_key.into(),
            provider,
            protection_key: Mutex::new(None),
        }
    }

    pub fn mode(&self) -> ProtectionMode {
        self.provider.mode()
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    fn protection_key(&self) -> Result<SecretBytes, VaultStoreError> {
        let mut slot = self
            .protection_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(key) = slot.as_ref() {
            return SecretBytes::try_clone(key).map_err(VaultStoreError::from);
        }
        let key = self.provider.load_or_create(&self.profile_key)?;
        if key.as_bytes().len() != 32 {
            return Err(VaultStoreError::Locked(
                "protection key has the wrong length".into(),
            ));
        }
        let clone = SecretBytes::try_clone(&key)?;
        *slot = Some(key);
        Ok(clone)
    }

    /// Load the state, or an empty state when no file exists yet.
    pub fn load(&self) -> Result<LocalVaultState, VaultStoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Touching the protection key on first use surfaces a
                // locked store before any secret is generated.
                self.protection_key()?;
                return Ok(LocalVaultState {
                    version: FILE_VERSION,
                    ..Default::default()
                });
            }
            Err(err) => return Err(err.into()),
        };
        if bytes.len() > MAX_STATE_BYTES {
            return Err(VaultStoreError::Corrupt("state file too large".into()));
        }
        let file: StateFile = serde_json::from_slice(&bytes)
            .map_err(|err| VaultStoreError::Corrupt(format!("state file: {err}")))?;
        if file.version != FILE_VERSION {
            return Err(VaultStoreError::Corrupt(format!(
                "unsupported vault state version {}",
                file.version
            )));
        }
        let key = self.protection_key()?;
        let nonce: [u8; 12] = file
            .nonce
            .decode()
            .ok_or_else(|| VaultStoreError::Corrupt("bad nonce".into()))?;
        let ciphertext = file
            .ciphertext
            .bytes()
            .ok_or_else(|| VaultStoreError::Corrupt("bad ciphertext".into()))?;
        let plaintext = zeron_crypto::open_aes256_gcm(
            key.as_bytes(),
            &nonce,
            FILE_AAD,
            &ciphertext,
            MAX_STATE_BYTES,
        )
        .map_err(|_| {
            VaultStoreError::Locked(
                "vault state does not decrypt under this device's protection key".into(),
            )
        })?;
        let state: LocalVaultState = serde_json::from_slice(plaintext.as_bytes())
            .map_err(|err| VaultStoreError::Corrupt(format!("state payload: {err}")))?;
        Ok(state)
    }

    /// Encrypt and atomically replace the state file (0600, fsync, rename).
    pub fn save(&self, state: &LocalVaultState) -> Result<(), VaultStoreError> {
        let key = self.protection_key()?;
        let plaintext = serde_json::to_vec(state)
            .map_err(|err| VaultStoreError::Corrupt(format!("serialize: {err}")))?;
        let mut nonce = [0u8; 12];
        zeron_crypto::fill_random(&mut nonce)?;
        let ciphertext =
            zeron_crypto::seal_aes256_gcm(key.as_bytes(), &nonce, FILE_AAD, &plaintext)?;
        let file = StateFile {
            version: FILE_VERSION,
            protection: self.provider.mode(),
            nonce: Hex::of(&nonce),
            ciphertext: Hex::of(&ciphertext),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| VaultStoreError::Corrupt(format!("serialize: {err}")))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        write_private(&temp, &bytes)?;
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

// ── providers ───────────────────────────────────────────────────────────────

/// Test-only: a random key held in memory for the process lifetime.
pub struct MemoryProtection(Mutex<Option<Vec<u8>>>);

impl MemoryProtection {
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }
}

impl Default for MemoryProtection {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtectionKeyProvider for MemoryProtection {
    fn mode(&self) -> ProtectionMode {
        ProtectionMode::Memory
    }

    fn load_or_create(&self, _profile_key: &str) -> Result<SecretBytes, VaultStoreError> {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            let mut key = vec![0u8; 32];
            zeron_crypto::fill_random(&mut key)?;
            *slot = Some(key);
        }
        Ok(SecretBytes::from_slice(slot.as_ref().expect("initialized")))
    }
}

/// Always locked: the platform offered no secure store. Keeps the encrypted
/// file untouched and reports why.
pub struct LockedProtection(pub String);

impl ProtectionKeyProvider for LockedProtection {
    fn mode(&self) -> ProtectionMode {
        ProtectionMode::Memory
    }

    fn load_or_create(&self, _profile_key: &str) -> Result<SecretBytes, VaultStoreError> {
        Err(VaultStoreError::Locked(self.0.clone()))
    }
}

/// Operator-provisioned key file (unattended mode): 64 hex chars or 32 raw
/// bytes, must be a regular file with mode 0600 owned by this process.
pub struct KeyFileProtection {
    path: PathBuf,
    mode: ProtectionMode,
}

impl KeyFileProtection {
    pub fn new(path: PathBuf, mode: ProtectionMode) -> Self {
        Self { path, mode }
    }
}

impl ProtectionKeyProvider for KeyFileProtection {
    fn mode(&self) -> ProtectionMode {
        self.mode
    }

    fn load_or_create(&self, _profile_key: &str) -> Result<SecretBytes, VaultStoreError> {
        let metadata = std::fs::metadata(&self.path).map_err(|err| {
            VaultStoreError::Locked(format!(
                "vault key file {} is not readable: {err}",
                self.path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(VaultStoreError::Locked(format!(
                "vault key file {} is not a regular file",
                self.path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(VaultStoreError::Locked(format!(
                    "vault key file {} must not be group/world accessible",
                    self.path.display()
                )));
            }
        }
        let raw = std::fs::read(&self.path)?;
        let key = if raw.len() == 32 {
            raw
        } else {
            let text = String::from_utf8_lossy(&raw);
            Hex(text.trim().to_string())
                .bytes()
                .filter(|k| k.len() == 32)
                .ok_or_else(|| {
                    VaultStoreError::Locked(format!(
                        "vault key file {} must hold 32 raw bytes or 64 hex characters",
                        self.path.display()
                    ))
                })?
        };
        Ok(SecretBytes::from_slice(&key))
    }
}

/// macOS login Keychain generic password via the `security` CLI (the same
/// primitive `agent_accounts` uses). Item service `sh.zeron.vault`, account
/// = the profile key, so each profile has its own device-bound key.
#[cfg(target_os = "macos")]
pub struct KeychainProtection;

#[cfg(target_os = "macos")]
impl ProtectionKeyProvider for KeychainProtection {
    fn mode(&self) -> ProtectionMode {
        ProtectionMode::Keychain
    }

    fn load_or_create(&self, profile_key: &str) -> Result<SecretBytes, VaultStoreError> {
        const SERVICE: &str = "sh.zeron.vault";
        let run = |args: &[&str]| -> Result<(bool, String), VaultStoreError> {
            let output = std::process::Command::new("security")
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|err| VaultStoreError::Locked(format!("security: {err}")))?;
            Ok((
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ))
        };
        let (found, existing) = run(&[
            "find-generic-password",
            "-a",
            profile_key,
            "-s",
            SERVICE,
            "-w",
        ])?;
        if found {
            return Hex(existing)
                .bytes()
                .filter(|k| k.len() == 32)
                .map(|k| SecretBytes::from_slice(&k))
                .ok_or_else(|| {
                    VaultStoreError::Locked("keychain item is not a 32-byte key".into())
                });
        }
        let mut key = [0u8; 32];
        zeron_crypto::fill_random(&mut key)?;
        let hex = Hex::of(&key).0;
        // No `-U`: never overwrite an item that appeared between the probe
        // and the add (a second engine racing us) — re-read instead.
        let (added, _) = run(&[
            "add-generic-password",
            "-a",
            profile_key,
            "-s",
            SERVICE,
            "-w",
            &hex,
        ])?;
        if !added {
            let (found, existing) = run(&[
                "find-generic-password",
                "-a",
                profile_key,
                "-s",
                SERVICE,
                "-w",
            ])?;
            if found {
                return Hex(existing)
                    .bytes()
                    .filter(|k| k.len() == 32)
                    .map(|k| SecretBytes::from_slice(&k))
                    .ok_or_else(|| {
                        VaultStoreError::Locked("keychain item is not a 32-byte key".into())
                    });
            }
            return Err(VaultStoreError::Locked(
                "macOS Keychain refused to store the vault protection key".into(),
            ));
        }
        Ok(SecretBytes::from_slice(&key))
    }
}

/// Pick the platform provider. Explicit credentials win (unattended mode),
/// then the macOS Keychain; anything else stays locked.
pub fn platform_protection() -> Box<dyn ProtectionKeyProvider> {
    if let Some(path) = std::env::var_os("ZERON_VAULT_KEY_FILE").filter(|p| !p.is_empty()) {
        return Box::new(KeyFileProtection::new(
            PathBuf::from(path),
            ProtectionMode::KeyFile,
        ));
    }
    if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(dir).join("zeron-vault-key");
        if path.exists() {
            return Box::new(KeyFileProtection::new(
                path,
                ProtectionMode::SystemdCredential,
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(KeychainProtection)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(LockedProtection(
            "no secure key store: provide ZERON_VAULT_KEY_FILE or a systemd credential \
             named zeron-vault-key (unattended mode)"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_encrypted_and_stays_sealed_without_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        assert_eq!(store.load().unwrap().device, None);
        let state = LocalVaultState {
            version: FILE_VERSION,
            device: Some(DeviceIdentity {
                device_id: Hex::of(&[1; 16]),
                signing_seed: Secret::of(&[2; 32]),
                encryption_secret: Secret::of(&[3; 32]),
            }),
            ..Default::default()
        };
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        let raw = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(
            !raw.contains(&Hex::of(&[2; 32]).0),
            "secrets never appear in the file"
        );
        // A different protection key cannot open the file and does not erase it.
        let other = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        assert!(matches!(other.load(), Err(VaultStoreError::Locked(_))));
        assert!(dir.path().join(STATE_FILE).exists());
        // A locked provider reports locked without touching the file.
        let locked = VaultStore::new(
            dir.path(),
            "org/user",
            Box::new(LockedProtection("no store".into())),
        );
        assert!(matches!(locked.load(), Err(VaultStoreError::Locked(_))));
        assert_eq!(
            format!("{:?}", state.device.as_ref().unwrap().signing_seed),
            "[REDACTED]"
        );
    }

    #[test]
    fn key_file_provider_enforces_permissions_and_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, Hex::of(&[7; 32]).0).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let provider = KeyFileProtection::new(path.clone(), ProtectionMode::KeyFile);
            assert!(matches!(
                provider.load_or_create("p"),
                Err(VaultStoreError::Locked(_))
            ));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let provider = KeyFileProtection::new(path.clone(), ProtectionMode::KeyFile);
        assert_eq!(provider.load_or_create("p").unwrap().as_bytes(), &[7; 32]);
        std::fs::write(&path, "not a key").unwrap();
        assert!(matches!(
            provider.load_or_create("p"),
            Err(VaultStoreError::Locked(_))
        ));
    }
}
