//! The vault lifecycle (RFC 0001 §4.3, §5, §6, §11): explicit states, the
//! verified membership history this device pins, its keyring, and the key
//! material it hands to content transports.
//!
//! Trust flows only from local pins: a genesis this device created, a
//! pairing the user confirmed by comparison code, or a recovery kit the user
//! typed. Server responses are verified against those pins; a server can
//! withhold data (availability) but cannot substitute trust.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::watch;
use zeron_crypto::content::{ContentKey, DeviceSigner, KeyScope};
use zeron_crypto::envelope::{self, RecipientKind};
use zeron_crypto::hpke::{HpkePrivateKey, HpkePublicKey};
use zeron_crypto::keyring::Keyring;
use zeron_crypto::policy::{
    self, DeviceStatus, EnrollmentRequest, MembershipState, Operation, POLICY_OBJECT_ID,
};
use zeron_crypto::record::{RecordBinding, RecordKind, UnverifiedRecord};
use zeron_crypto::recovery::RecoverySecret;

use super::client::{VaultClient, decode_base64, encode_base64};
use super::store::{
    CachedObjectKey, DeviceIdentity, Hex, LocalVaultState, PendingEnrollment, PinnedVault,
    ProtectionMode, Secret, VaultStore, VaultStoreError,
};
use crate::EngineError;

const OBJECT_ID_DOMAIN: &[u8] = b"zeron/object-id/v1\0";
const MAX_MEMBERSHIP_RECORDS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum VaultPhase {
    /// Local-only profile or no edge: encryption does not apply.
    Unavailable {
        reason: String,
    },
    /// Secure key storage could not be opened; keys are preserved.
    Locked {
        reason: String,
    },
    /// This device has no membership. `remote_vault` says whether one
    /// already exists for the profile (approve/recover) or not (set up).
    NotEnrolled {
        remote_vault: bool,
    },
    /// An enrollment request awaits approval on another device.
    Pending {
        request_id: String,
        pairing_code: String,
        expires_at: i64,
    },
    Ready,
    RecoveryConfirmationRequired,
    /// Membership advanced to an epoch whose key this device does not hold.
    KeyUpdateRequired,
    /// Server data failed verification against local pins.
    VerificationFailed {
        reason: String,
    },
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultDevice {
    pub device_id: String,
    pub status: String,
    pub this_device: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequest {
    pub request_id: String,
    pub device_id: String,
    pub pairing_code: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultStatus {
    #[serde(flatten)]
    pub phase: VaultPhase,
    pub vault_id: Option<String>,
    pub device_id: Option<String>,
    pub epoch: Option<u64>,
    pub devices: Vec<VaultDevice>,
    pub protection: ProtectionMode,
}

/// The recovery kit shown once at setup / rotation (RFC §4.1). The kit text
/// is the secret; the file carries only public trust-anchor metadata.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryKit {
    pub kit: String,
    pub recovery_file: serde_json::Value,
}

impl std::fmt::Debug for RecoveryKit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecoveryKit([REDACTED])")
    }
}

/// Everything a writer needs to seal one record for an object under the
/// current epoch.
pub struct ChatKeyMaterial {
    pub binding: RecordBinding,
    pub key: Arc<ContentKey>,
    pub signer: Arc<DeviceSigner>,
}

/// Everything a reader needs to open one record whose untrusted binding it
/// has parsed: the TRUSTED binding rebuilt from pinned history, the key, and
/// the author's public key at that revision.
pub struct OpenContext {
    pub binding: RecordBinding,
    pub key: Arc<ContentKey>,
    pub author_public_key: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFailure {
    /// Vault not ready on this device (locked, not enrolled, revoked).
    Unavailable,
    /// The record names an epoch / object key this device does not hold yet.
    KeyUnavailable,
    /// The record's membership hash / author / epoch are not in trusted history.
    NotAuthorized,
}

#[derive(Clone)]
struct Revision {
    hash: [u8; 32],
    state: MembershipState,
}

struct Trust {
    device_id: [u8; 16],
    signer: Arc<DeviceSigner>,
    encryption: HpkePrivateKey,
    history: Vec<Revision>,
    keyring: Keyring,
    object_keys: BTreeMap<([u8; 16], u64), Arc<ContentKey>>,
}

impl Trust {
    fn head(&self) -> &MembershipState {
        &self.history.last().expect("non-empty history").state
    }

    fn revision(&self, hash: &[u8; 32]) -> Option<&Revision> {
        self.history.iter().rev().find(|r| r.hash == *hash)
    }

    fn signing_key(&self) -> [u8; 32] {
        self.signer.public_key().try_into().expect("32-byte key")
    }
}

struct Guarded {
    encryption_required: bool,
    state: LocalVaultState,
    trust: Option<Trust>,
    locked: Option<String>,
    verification_failure: Option<String>,
    remote_vault: Option<bool>,
}

impl Guarded {
    fn content_blocked(&self) -> bool {
        self.locked.is_some()
            || self.verification_failure.is_some()
            || self.state.pending_membership.is_some()
            || self.state.setup_recovery.is_some()
            || !self.state.owed_envelopes.is_empty()
    }
}

struct Inner {
    store: VaultStore,
    client: Option<VaultClient>,
    profile_hash: [u8; 32],
    guarded: Mutex<Guarded>,
    /// Serializes network-mutating operations (setup/approve/revoke/refresh).
    ops: tokio::sync::Mutex<()>,
    status: watch::Sender<VaultStatus>,
}

#[derive(Clone)]
pub struct VaultService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for VaultService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "VaultService({:?})", self.status().phase)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// (trusted binding, author public key, epoch key held, cached object key).
type TrustedContext = (RecordBinding, [u8; 32], bool, Option<Arc<ContentKey>>);

/// Opaque 16-byte object id for a chat/registry/blob identifier.
pub fn object_id_for(kind: &str, id: &str) -> [u8; 16] {
    let digest = zeron_crypto::sha256(&[OBJECT_ID_DOMAIN, kind.as_bytes(), b"\0", id.as_bytes()]);
    let mut out = [0; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

impl VaultService {
    /// Open the local store and rebuild trust. Never panics on a locked or
    /// corrupt store: the status says so and the file is left untouched.
    pub fn open(
        store: VaultStore,
        client: Option<VaultClient>,
        org_id: &str,
        user_id: &str,
    ) -> Self {
        let profile_hash = policy::profile_hash(org_id, user_id);
        let (state, locked) = match store.load() {
            Ok(state) => (state, None),
            Err(VaultStoreError::Locked(reason)) => (LocalVaultState::default(), Some(reason)),
            Err(err) => (LocalVaultState::default(), Some(err.to_string())),
        };
        let mut guarded = Guarded {
            encryption_required: store.exists(),
            state,
            trust: None,
            locked,
            verification_failure: None,
            remote_vault: None,
        };
        if guarded.locked.is_none() {
            match rebuild_trust(&guarded.state, &profile_hash) {
                Ok(trust) => guarded.trust = trust,
                Err(reason) => guarded.verification_failure = Some(reason),
            }
        }
        let (status, _) = watch::channel(VaultStatus {
            phase: VaultPhase::Unavailable {
                reason: "starting".into(),
            },
            vault_id: None,
            device_id: None,
            epoch: None,
            devices: Vec::new(),
            protection: store.mode(),
        });
        let service = Self {
            inner: Arc::new(Inner {
                store,
                client,
                profile_hash,
                guarded: Mutex::new(guarded),
                ops: tokio::sync::Mutex::new(()),
                status,
            }),
        };
        service.publish_status();
        service
    }

    pub fn status(&self) -> VaultStatus {
        self.inner.status.borrow().clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<VaultStatus> {
        self.inner.status.subscribe()
    }

    /// True when content transports must seal/open (this device is an
    /// approved member with a usable keyring).
    pub fn is_ready(&self) -> bool {
        matches!(self.status().phase, VaultPhase::Ready)
    }

    /// True when the profile is enrolled in a vault at all: a member whose
    /// keys are stale, locked, or revoked must still never write plaintext.
    pub fn is_enrolled(&self) -> bool {
        let guarded = lock(&self.inner.guarded);
        guarded.encryption_required
            || guarded.state.vault.is_some()
            || guarded.remote_vault == Some(true)
    }

    pub fn device_id(&self) -> Option<[u8; 16]> {
        lock(&self.inner.guarded)
            .trust
            .as_ref()
            .map(|t| t.device_id)
    }

    fn publish_status(&self) {
        let status = {
            let guarded = lock(&self.inner.guarded);
            self.compute_status(&guarded)
        };
        self.inner.status.send_replace(status);
    }

    fn compute_status(&self, guarded: &Guarded) -> VaultStatus {
        let protection = self.inner.store.mode();
        let device_id = guarded
            .state
            .device
            .as_ref()
            .map(|d| d.device_id.0.clone())
            .or_else(|| guarded.trust.as_ref().map(|t| Hex::of(&t.device_id).0));
        let base = |phase: VaultPhase| VaultStatus {
            phase,
            vault_id: guarded.state.vault.as_ref().map(|v| v.vault_id.0.clone()),
            device_id: device_id.clone(),
            epoch: None,
            devices: Vec::new(),
            protection,
        };
        if self.inner.client.is_none() {
            return base(VaultPhase::Unavailable {
                reason: "this profile does not sync".into(),
            });
        }
        if let Some(reason) = &guarded.locked {
            return base(VaultPhase::Locked {
                reason: reason.clone(),
            });
        }
        if let Some(reason) = &guarded.verification_failure {
            return base(VaultPhase::VerificationFailed {
                reason: reason.clone(),
            });
        }
        if guarded.state.pending_membership.is_some() {
            return base(VaultPhase::KeyUpdateRequired);
        }
        let Some(trust) = &guarded.trust else {
            if let Some(pending) = &guarded.state.enrollment {
                let code = guarded
                    .state
                    .vault
                    .as_ref()
                    .and_then(|v| v.membership.first())
                    .and_then(|g| decode_base64(g))
                    .map(|g| policy::membership_hash(&g))
                    .and_then(|genesis| {
                        pending_request(&guarded.state, &pending.request_id)
                            .map(|r| r.pairing_code(&genesis))
                    })
                    .unwrap_or_default();
                return base(VaultPhase::Pending {
                    request_id: pending.request_id.0.clone(),
                    pairing_code: code,
                    expires_at: pending.created_at + 15 * 60 * 1000,
                });
            }
            return base(VaultPhase::NotEnrolled {
                remote_vault: guarded.remote_vault.unwrap_or(false),
            });
        };
        let head = trust.head();
        let devices = head
            .devices()
            .iter()
            .map(|d| VaultDevice {
                device_id: Hex::of(&d.device_id).0,
                status: match d.status {
                    DeviceStatus::Active => "active".into(),
                    DeviceStatus::Revoked => "revoked".into(),
                },
                this_device: d.device_id == trust.device_id,
            })
            .collect();
        let phase = if head.active_device(&trust.device_id).is_none() {
            VaultPhase::Revoked
        } else if trust.keyring.epoch_key(head.epoch()).is_none()
            || !guarded.state.owed_envelopes.is_empty()
        {
            VaultPhase::KeyUpdateRequired
        } else if guarded.state.setup_recovery.is_some() {
            VaultPhase::RecoveryConfirmationRequired
        } else {
            VaultPhase::Ready
        };
        VaultStatus {
            phase,
            vault_id: Some(Hex::of(head.vault_id()).0),
            device_id,
            epoch: Some(head.epoch()),
            devices,
            protection,
        }
    }

    fn client(&self) -> Result<&VaultClient, EngineError> {
        self.inner
            .client
            .as_ref()
            .ok_or_else(|| EngineError::Other("this profile does not sync".into()))
    }

    fn commit(&self, guarded: &mut Guarded) -> Result<(), EngineError> {
        if let Err(error) = self.inner.store.save(&guarded.state) {
            guarded.encryption_required = true;
            guarded.locked = Some(error.to_string());
            self.inner.status.send_replace(self.compute_status(guarded));
            return Err(error.into());
        }
        self.inner.status.send_replace(self.compute_status(guarded));
        Ok(())
    }

    async fn finish_pending_membership(&self, client: &VaultClient) -> Result<(), EngineError> {
        let pending = {
            let guarded = lock(&self.inner.guarded);
            if guarded.state.pending_membership.is_none()
                && guarded.state.owed_envelopes.is_empty()
                && guarded.state.pending_approval.is_none()
            {
                return Ok(());
            }
            guarded.state.pending_membership.clone()
        };
        if let Some(encoded) = pending {
            let (record, sequence) = {
                let guarded = lock(&self.inner.guarded);
                if guarded.locked.is_some() || guarded.verification_failure.is_some() {
                    return Err(EngineError::Other("vault is locked or unverified".into()));
                }
                let candidate = rebuild_trust(&guarded.state, &self.inner.profile_hash)
                    .map_err(EngineError::Other)?
                    .ok_or_else(|| {
                        EngineError::Other("pending membership has no trust anchor".into())
                    })?;
                let record = decode_base64(&encoded)
                    .ok_or_else(|| EngineError::Other("invalid pending membership".into()))?;
                if policy::membership_hash(&record) != *candidate.head().hash() {
                    return Err(EngineError::Other(
                        "pending membership does not match the journal".into(),
                    ));
                }
                (record, candidate.head().sequence())
            };
            if !matches!(client.post_membership(record).await, Ok(Ok(_))) {
                let after = i64::try_from(sequence)
                    .map_err(|_| EngineError::Other("membership sequence overflow".into()))?
                    - 1;
                let page = client.membership_after(after).await?;
                if page.records.first() != Some(&encoded) {
                    return Err(EngineError::Other(
                        "pending membership was not accepted; journal retained".into(),
                    ));
                }
            }
            let mut guarded = lock(&self.inner.guarded);
            guarded.trust = rebuild_trust(&guarded.state, &self.inner.profile_hash)
                .map_err(EngineError::Other)?;
            guarded.state.pending_membership = None;
            self.commit(&mut guarded)?;
        }
        self.pull_membership(client).await?;
        self.pull_keyring(client).await?;
        self.publish_owed(client).await?;
        let approval = lock(&self.inner.guarded).state.pending_approval.clone();
        if !lock(&self.inner.guarded).state.owed_envelopes.is_empty() {
            return Err(EngineError::Other(
                "key envelopes are still pending; refresh to retry".into(),
            ));
        }
        if let Some((request, sequence)) = approval {
            let request = request
                .decode::<16>()
                .ok_or_else(|| EngineError::Other("invalid pending approval".into()))?;
            client.approve_enrollment(&request, sequence).await?;
            let mut guarded = lock(&self.inner.guarded);
            guarded.state.pending_approval = None;
            self.commit(&mut guarded)?;
        }
        self.publish_status();
        Ok(())
    }

    fn setup_kit(&self) -> Result<RecoveryKit, EngineError> {
        let guarded = lock(&self.inner.guarded);
        if guarded.locked.is_some() || guarded.verification_failure.is_some() {
            return Err(EngineError::Other("vault is locked or unverified".into()));
        }
        let secret = guarded
            .state
            .setup_recovery
            .as_ref()
            .and_then(Secret::bytes)
            .ok_or_else(|| EngineError::Other("no recovery kit awaits confirmation".into()))?;
        let recovery =
            RecoverySecret::from_bytes(&secret).map_err(|e| EngineError::Other(e.to_string()))?;
        let trust = guarded
            .trust
            .as_ref()
            .ok_or_else(|| EngineError::Other("vault setup is pending".into()))?;
        Ok(RecoveryKit {
            kit: recovery.to_kit().to_string(),
            recovery_file: recovery_file(trust.head()),
        })
    }

    pub async fn confirm_recovery_kit(&self) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        if matches!(self.status().phase, VaultPhase::Ready) {
            return Ok(());
        }
        if !matches!(
            self.status().phase,
            VaultPhase::RecoveryConfirmationRequired
        ) {
            return Err(EngineError::Other(
                "recovery kit is not ready for confirmation".into(),
            ));
        }
        let mut guarded = lock(&self.inner.guarded);
        guarded.state.setup_recovery = None;
        self.commit(&mut guarded)
    }

    /// Ensure this device has a private identity (generated once, persisted
    /// before any use so a crash cannot produce two identities).
    fn ensure_identity(&self, guarded: &mut Guarded) -> Result<Identity, EngineError> {
        if let Some(reason) = &guarded.locked {
            return Err(EngineError::Other(format!("vault locked: {reason}")));
        }
        if guarded.state.device.is_none() {
            let mut device_id = [0u8; 16];
            let mut seed = [0u8; 32];
            zeron_crypto::fill_random(&mut device_id)
                .map_err(|e| EngineError::Other(e.to_string()))?;
            zeron_crypto::fill_random(&mut seed).map_err(|e| EngineError::Other(e.to_string()))?;
            let encryption =
                HpkePrivateKey::generate().map_err(|e| EngineError::Other(e.to_string()))?;
            guarded.state.device = Some(DeviceIdentity {
                device_id: Hex::of(&device_id),
                signing_seed: Secret::of(&seed),
                encryption_secret: Secret::of(encryption.expose_secret()),
            });
            guarded.state.version = 1;
            self.commit(guarded)?;
        }
        identity_of(&guarded.state)
    }

    // ── setup ────────────────────────────────────────────────────────────────

    /// Create the vault for this profile with this device as its first
    /// member. Fails if a vault already exists (approve or recover instead).
    pub async fn setup(&self) -> Result<RecoveryKit, EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        let resuming = lock(&self.inner.guarded).state.setup_recovery.is_some();
        if resuming {
            self.finish_pending_membership(&client).await?;
            return self.setup_kit();
        }
        if lock(&self.inner.guarded).trust.is_some() {
            return Err(EngineError::Other("this device is already enrolled".into()));
        }
        if client.descriptor().await?.is_some() {
            lock(&self.inner.guarded).remote_vault = Some(true);
            self.publish_status();
            return Err(EngineError::Other(
                "a vault already exists for this account; approve this device from another device or use your recovery key"
                    .into(),
            ));
        }
        let identity = {
            let mut guarded = lock(&self.inner.guarded);
            self.ensure_identity(&mut guarded)?
        };
        let recovery = RecoverySecret::generate().map_err(|e| EngineError::Other(e.to_string()))?;
        let recovery_signer = recovery
            .signer()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let recovery_encryption = recovery
            .encryption_key()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let mut vault_id = [0u8; 16];
        let mut generation = [0u8; 16];
        zeron_crypto::fill_random(&mut vault_id).map_err(|e| EngineError::Other(e.to_string()))?;
        zeron_crypto::fill_random(&mut generation)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let payload = policy::PolicyPayload {
            sequence: 0,
            parent_hash: [0; 32],
            profile_hash: self.inner.profile_hash,
            epoch: 1,
            operation: Operation::Genesis,
            recovery_signing_key: recovery_signer
                .public_key()
                .try_into()
                .map_err(|_| EngineError::Other("recovery key".into()))?,
            recovery_encryption_key: *recovery_encryption.public_key().as_bytes(),
            devices: vec![identity.entry()],
        };
        let genesis = policy::encode_policy(
            &policy::policy_binding(vault_id, generation, 1, identity.device_id, [0; 32]),
            &payload,
            &identity.signer,
        )
        .map_err(|e| EngineError::Other(format!("genesis: {e}")))?;
        let state = MembershipState::from_genesis(
            &genesis,
            &vault_id,
            &generation,
            &self.inner.profile_hash,
        )
        .map_err(|e| EngineError::Other(format!("genesis verify: {e}")))?;
        let mut keyring = Keyring::new();
        keyring
            .insert_fresh(1)
            .map_err(|e| EngineError::Other(e.to_string()))?;

        // Publish order (RFC §5): journal keys and genesis before sending
        // the membership record, then deliver the recipient envelopes.
        // Recovery material stays protected locally until the user confirms
        // saving the kit. Interrupted publication resumes the same intent;
        // content remains paused until delivery and confirmation complete.
        {
            let mut guarded = lock(&self.inner.guarded);
            guarded.state.vault = Some(PinnedVault {
                vault_id: Hex::of(&vault_id),
                generation: Hex::of(&generation),
                profile_hash: Hex::of(&self.inner.profile_hash),
                membership: vec![encode_base64(&genesis)],
            });
            guarded.state.keyring = Some(Secret::of(&keyring.encode()));
            guarded.state.enrollment = None;
            guarded.state.pending_membership = Some(encode_base64(&genesis));
            guarded.state.setup_recovery = Some(Secret::of(recovery.expose_secret()));
            guarded.state.owed_envelopes = vec![
                (Hex::of(&identity.device_id), 1),
                (Hex::of(&state.recovery_authority_id()), 1),
            ];
            self.commit(&mut guarded)?;
            guarded.remote_vault = Some(true);
        }
        self.finish_pending_membership(&client).await?;
        self.setup_kit()
    }

    // ── refresh ──────────────────────────────────────────────────────────────

    /// Reconcile with the control plane: pull new membership records, learn
    /// of revocation, fetch a keyring update, publish owed envelopes, and
    /// complete a pending enrollment.
    pub async fn refresh(&self) -> Result<VaultStatus, EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        if lock(&self.inner.guarded).locked.is_some() {
            return Ok(self.status());
        }
        self.finish_pending_membership(&client).await?;
        let descriptor = client.descriptor().await?;
        {
            let mut guarded = lock(&self.inner.guarded);
            let discovered = descriptor.is_some() && guarded.remote_vault != Some(true);
            guarded.remote_vault = Some(descriptor.is_some() || guarded.remote_vault == Some(true));
            if discovered {
                guarded.encryption_required = true;
                guarded.state.version = 1;
                self.commit(&mut guarded)?;
            }
        }
        let enrolled = lock(&self.inner.guarded).trust.is_some();
        if enrolled {
            self.pull_membership(&client).await?;
            self.pull_keyring(&client).await?;
            self.publish_owed(&client).await?;
        } else if lock(&self.inner.guarded).state.enrollment.is_some() {
            self.poll_enrollment(&client).await?;
        }
        self.publish_status();
        Ok(self.status())
    }

    async fn pull_membership(&self, client: &VaultClient) -> Result<(), EngineError> {
        let head_seq = lock(&self.inner.guarded)
            .trust
            .as_ref()
            .map(|t| t.head().sequence() as i64)
            .unwrap_or(-1);
        let page = client.membership_after(head_seq).await?;
        if page.records.is_empty() {
            return Ok(());
        }
        let mut guarded = lock(&self.inner.guarded);
        let Some(trust) = guarded.trust.as_mut() else {
            return Ok(());
        };
        let mut appended = Vec::new();
        for encoded in &page.records {
            let bytes = decode_base64(encoded)
                .ok_or_else(|| EngineError::Other("malformed membership record".into()))?;
            match trust.head().apply(&bytes) {
                Ok(next) => {
                    trust.history.push(Revision {
                        hash: *next.hash(),
                        state: next,
                    });
                    appended.push(encoded.clone());
                }
                Err(err) => {
                    // A record the server serves that our pinned history
                    // rejects: stop advancing and surface it. Nothing local
                    // is discarded.
                    guarded.verification_failure =
                        Some(format!("membership record rejected: {err}"));
                    break;
                }
            }
        }
        if !appended.is_empty() {
            if let Some(vault) = guarded.state.vault.as_mut() {
                vault.membership.extend(appended);
                if vault.membership.len() > MAX_MEMBERSHIP_RECORDS {
                    guarded.verification_failure = Some("membership history too long".into());
                }
            }
            self.commit(&mut guarded)?;
        }
        Ok(())
    }

    /// Fetch this device's keyring envelope when the head epoch is ahead of
    /// what the keyring holds.
    async fn pull_keyring(&self, client: &VaultClient) -> Result<(), EngineError> {
        let (device_id, needs) = {
            let guarded = lock(&self.inner.guarded);
            let Some(trust) = guarded.trust.as_ref() else {
                return Ok(());
            };
            let head = trust.head();
            (
                trust.device_id,
                head.active_device(&trust.device_id).is_some()
                    && trust.keyring.epoch_key(head.epoch()).is_none(),
            )
        };
        if !needs {
            return Ok(());
        }
        let Some(envelope) = client.get_envelope(&device_id).await? else {
            return Ok(());
        };
        let mut guarded = lock(&self.inner.guarded);
        let Some(trust) = guarded.trust.as_mut() else {
            return Ok(());
        };
        match open_keyring_envelope(trust, &envelope, RecipientKind::Device, &device_id) {
            Ok(keyring) => {
                trust
                    .keyring
                    .merge(&keyring)
                    .map_err(|e| EngineError::Other(format!("keyring merge: {e}")))?;
                guarded.state.keyring = Some(Secret::of(&trust.keyring.encode()));
                self.commit(&mut guarded)?;
            }
            Err(reason) => {
                tracing::warn!(reason, "vault: keyring envelope rejected");
            }
        }
        Ok(())
    }

    /// Envelopes this device promised after a rotation it authored.
    async fn publish_owed(&self, client: &VaultClient) -> Result<(), EngineError> {
        let owed = lock(&self.inner.guarded).state.owed_envelopes.clone();
        let mut done = Vec::new();
        for (recipient, epoch) in owed {
            let recipient_id = recipient
                .decode::<16>()
                .ok_or_else(|| EngineError::Other("invalid owed recipient".into()))?;
            let slot = format!("{}:{epoch}", recipient.0);
            let sealed = {
                let mut guarded = lock(&self.inner.guarded);
                if guarded.locked.is_some()
                    || guarded.verification_failure.is_some()
                    || guarded.state.pending_membership.is_some()
                {
                    return Err(EngineError::Other(
                        "vault update is not durable or verified".into(),
                    ));
                }
                let trust = guarded
                    .trust
                    .as_ref()
                    .ok_or_else(|| EngineError::Other("vault keys are unavailable".into()))?;
                let cached = guarded
                    .state
                    .owed_envelope_records
                    .get(&slot)
                    .and_then(|encoded| decode_base64(encoded))
                    .filter(|bytes| {
                        UnverifiedRecord::parse(bytes, envelope::MAX_ENVELOPE_PAYLOAD_BYTES)
                            .is_ok_and(|record| {
                                record.untrusted_binding().membership_hash == *trust.head().hash()
                            })
                    });
                let sealed = match cached {
                    Some(record) => Some(Ok(record)),
                    None => seal_keyring_for(trust, &recipient_id, epoch),
                };
                if let Some(Ok(record)) = &sealed {
                    let encoded = encode_base64(record);
                    if guarded.state.owed_envelope_records.get(&slot) != Some(&encoded) {
                        guarded.state.owed_envelope_records.insert(slot, encoded);
                        self.commit(&mut guarded)?;
                    }
                }
                sealed
            };
            match sealed {
                Some(Ok(record)) => match client.put_envelope(&recipient_id, record).await {
                    Ok(()) => done.push((recipient, epoch)),
                    Err(err) => tracing::warn!(error = %err, "vault: owed envelope publish failed"),
                },
                Some(Err(reason)) => return Err(EngineError::Other(reason)),
                None => done.push((recipient, epoch)),
            }
        }
        if !done.is_empty() {
            let mut guarded = lock(&self.inner.guarded);
            guarded
                .state
                .owed_envelopes
                .retain(|entry| !done.contains(entry));
            for (recipient, epoch) in done {
                guarded
                    .state
                    .owed_envelope_records
                    .remove(&format!("{}:{epoch}", recipient.0));
            }
            self.commit(&mut guarded)?;
        }
        Ok(())
    }

    // ── enrollment (new device) ──────────────────────────────────────────────

    /// Ask an existing device to approve this one. Returns the request id
    /// and the comparison code the user must match on the approving device.
    pub async fn request_enrollment(&self) -> Result<(String, String), EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        if lock(&self.inner.guarded).trust.is_some() {
            return Err(EngineError::Other("this device is already enrolled".into()));
        }
        let Some(descriptor) = client.descriptor().await? else {
            return Err(EngineError::Other(
                "no vault exists for this account yet; set up encryption first".into(),
            ));
        };
        let vault_id = descriptor
            .vault_id
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad vault id".into()))?;
        let generation = descriptor
            .generation
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad generation".into()))?;
        // The chain is fetched now (unverified against any pin) so the
        // pairing code can bind its genesis hash. It is pinned only once the
        // approver's decision — made against ITS pinned genesis — lands as a
        // membership record that admits exactly our keys.
        let records = fetch_all_membership(&client).await?;
        let genesis_hash = policy::membership_hash(
            &records
                .first()
                .and_then(|r| decode_base64(r))
                .ok_or_else(|| EngineError::Other("empty membership".into()))?,
        );
        verify_chain(&records, &vault_id, &generation, &self.inner.profile_hash)
            .map_err(|reason| EngineError::Other(format!("remote vault rejected: {reason}")))?;
        let identity = {
            let mut guarded = lock(&self.inner.guarded);
            self.ensure_identity(&mut guarded)?
        };
        let mut request_id = [0u8; 16];
        zeron_crypto::fill_random(&mut request_id)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let request = EnrollmentRequest {
            vault_id,
            request_id,
            device_id: identity.device_id,
            signing_key: identity.signing_key(),
            encryption_key: *identity.encryption.public_key().as_bytes(),
        };
        let proof = request
            .sign(&identity.signer)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let info = client.create_enrollment(&request, &proof).await?;
        let code = request.pairing_code(&genesis_hash);
        {
            let mut guarded = lock(&self.inner.guarded);
            guarded.state.vault = Some(PinnedVault {
                vault_id: Hex::of(&vault_id),
                generation: Hex::of(&generation),
                profile_hash: Hex::of(&self.inner.profile_hash),
                membership: records,
            });
            guarded.state.enrollment = Some(PendingEnrollment {
                request_id: Hex::of(&request_id),
                created_at: info.created_at,
            });
            guarded.remote_vault = Some(true);
            self.commit(&mut guarded)?;
        }
        self.publish_status();
        Ok((Hex::of(&request_id).0, code))
    }

    pub async fn cancel_enrollment(&self) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        let pending = lock(&self.inner.guarded).state.enrollment.clone();
        if let (Some(pending), Ok(client)) = (pending, self.client())
            && let Some(id) = pending.request_id.decode::<16>()
        {
            let _ = client.reject_enrollment(&id).await;
        }
        let mut guarded = lock(&self.inner.guarded);
        guarded.state.enrollment = None;
        if guarded.trust.is_none() {
            guarded.state.vault = None;
        }
        self.commit(&mut guarded)?;
        drop(guarded);
        self.publish_status();
        Ok(())
    }

    async fn poll_enrollment(&self, client: &VaultClient) -> Result<(), EngineError> {
        let (pending, identity) = {
            let guarded = lock(&self.inner.guarded);
            let Some(pending) = guarded.state.enrollment.clone() else {
                return Ok(());
            };
            (pending, identity_of(&guarded.state)?)
        };
        let Some(request_id) = pending.request_id.decode::<16>() else {
            return Ok(());
        };
        let info = client.enrollment(&request_id).await?;
        match info.as_ref().map(|i| i.status.as_str()) {
            Some("approved") => {}
            Some("pending") => return Ok(()),
            _ => {
                // Rejected, expired, or gone: clear the request; the user may
                // ask again. Our identity stays (nothing about it leaked).
                let mut guarded = lock(&self.inner.guarded);
                guarded.state.enrollment = None;
                guarded.state.vault = None;
                self.commit(&mut guarded)?;
                return Ok(());
            }
        }
        let (vault_id, generation) = {
            let guarded = lock(&self.inner.guarded);
            let vault = guarded
                .state
                .vault
                .as_ref()
                .ok_or_else(|| EngineError::Other("missing pinned vault".into()))?;
            (
                vault
                    .vault_id
                    .decode::<16>()
                    .ok_or_else(|| EngineError::Other("bad vault".into()))?,
                vault
                    .generation
                    .decode::<16>()
                    .ok_or_else(|| EngineError::Other("bad vault".into()))?,
            )
        };
        let records = fetch_all_membership(client).await?;
        let head = verify_chain(&records, &vault_id, &generation, &self.inner.profile_hash)
            .map_err(|reason| EngineError::Other(format!("remote vault rejected: {reason}")))?;
        let entry = head
            .active_device(&identity.device_id)
            .ok_or_else(|| EngineError::Other("approval did not admit this device".into()))?;
        if entry.signing_key != identity.signing_key()
            || entry.encryption_key != *identity.encryption.public_key().as_bytes()
        {
            return Err(EngineError::Other(
                "membership names this device with different keys; refusing".into(),
            ));
        }
        let Some(envelope) = client.get_envelope(&identity.device_id).await? else {
            return Ok(()); // approver still publishing; try again later
        };
        let mut guarded = lock(&self.inner.guarded);
        guarded.state.vault = Some(PinnedVault {
            vault_id: Hex::of(&vault_id),
            generation: Hex::of(&generation),
            profile_hash: Hex::of(&self.inner.profile_hash),
            membership: records,
        });
        // An empty keyring makes the pinned history rebuildable; the real
        // keyring replaces it once the envelope opens against that history.
        guarded.state.keyring = Some(Secret::of(&Keyring::new().encode()));
        let mut trust = rebuild_trust(&guarded.state, &self.inner.profile_hash)
            .map_err(EngineError::Other)?
            .ok_or_else(|| EngineError::Other("trust rebuild failed".into()))?;
        let keyring = match open_keyring_envelope(
            &trust,
            &envelope,
            RecipientKind::Device,
            &identity.device_id,
        ) {
            Ok(keyring) => keyring,
            Err(reason) => {
                guarded.state.keyring = None;
                return Err(EngineError::Other(format!(
                    "keyring envelope rejected: {reason}"
                )));
            }
        };
        trust.keyring = keyring;
        guarded.state.keyring = Some(Secret::of(&trust.keyring.encode()));
        guarded.state.enrollment = None;
        self.commit(&mut guarded)?;
        guarded.trust = Some(trust);
        Ok(())
    }

    // ── approval (existing device) ───────────────────────────────────────────

    /// Pending requests with the comparison code computed from OUR pinned
    /// genesis and the keys the server reports (the user compares it against
    /// the code shown on the pending device).
    pub async fn pending_requests(&self) -> Result<Vec<PendingRequest>, EngineError> {
        let client = self.client()?.clone();
        let (vault_id, genesis) = {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("this device is not enrolled".into()))?;
            (*trust.head().vault_id(), *trust.head().genesis_hash())
        };
        let mut out = Vec::new();
        for info in client.list_enrollments().await? {
            let Some(request) = request_from_info(&info, vault_id) else {
                continue;
            };
            out.push(PendingRequest {
                request_id: info.request_id.0.clone(),
                device_id: info.device_id.0.clone(),
                pairing_code: request.pairing_code(&genesis),
                expires_at: info.expires_at,
            });
        }
        Ok(out)
    }

    /// Admit a pending device. `confirmed_code` must equal the code the
    /// user read from the pending device; it is re-derived here from the
    /// server-reported keys so a substituted key cannot pass.
    pub async fn approve(
        &self,
        request_id_hex: &str,
        confirmed_code: &str,
    ) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        let request_id = Hex(request_id_hex.to_string())
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad request id".into()))?;
        let info = client
            .enrollment(&request_id)
            .await?
            .ok_or_else(|| EngineError::Other("enrollment request not found".into()))?;
        if info.status != "pending" {
            return Err(EngineError::Other(format!("request is {}", info.status)));
        }
        let (vault_id, genesis, identity) = {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("this device is not enrolled".into()))?;
            (
                *trust.head().vault_id(),
                *trust.head().genesis_hash(),
                identity_of(&guarded.state)?,
            )
        };
        let request = request_from_info(&info, vault_id)
            .ok_or_else(|| EngineError::Other("malformed enrollment request".into()))?;
        let expected = request.pairing_code(&genesis);
        if normalize_code(confirmed_code) != normalize_code(&expected) {
            return Err(EngineError::Other(
                "the code does not match; do not approve a device whose code differs".into(),
            ));
        }
        // Make sure our view of the head is current before authoring on it.
        self.finish_pending_membership(&client).await?;
        self.pull_membership(&client).await?;
        if !self.is_ready() {
            return Err(EngineError::Other("vault is not ready for approval".into()));
        }
        let (record, next_state) = {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("not enrolled".into()))?;
            let head = trust.head();
            if head.active_device(&trust.device_id).is_none() {
                return Err(EngineError::Other("this device is revoked".into()));
            }
            let mut payload = head.next_payload(Operation::AddDevice);
            payload.devices.push(request.device_entry());
            let binding = policy::policy_binding(
                *head.vault_id(),
                *head.generation(),
                payload.epoch,
                trust.device_id,
                *head.hash(),
            );
            let record = policy::encode_policy(&binding, &payload, &trust.signer)
                .map_err(|e| EngineError::Other(format!("add device: {e}")))?;
            let next = head
                .apply(&record)
                .map_err(|e| EngineError::Other(format!("add device verify: {e}")))?;
            (record, next)
        };
        let _ = identity;
        {
            let mut guarded = lock(&self.inner.guarded);
            if let Some(vault) = guarded.state.vault.as_mut() {
                vault.membership.push(encode_base64(&record));
            }
            if let Some(trust) = guarded.trust.as_mut() {
                trust.history.push(Revision {
                    hash: *next_state.hash(),
                    state: next_state.clone(),
                });
            }
            // The envelope is owed until it lands; a crash here retries it.
            guarded
                .state
                .owed_envelopes
                .push((Hex::of(&request.device_id), next_state.epoch()));
            guarded.state.pending_membership = Some(encode_base64(&record));
            guarded.state.pending_approval =
                Some((Hex::of(&request_id), next_state.sequence() as i64));
            self.commit(&mut guarded)?;
        }
        self.finish_pending_membership(&client).await
    }

    pub async fn reject(&self, request_id_hex: &str) -> Result<(), EngineError> {
        let client = self.client()?.clone();
        let request_id = Hex(request_id_hex.to_string())
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad request id".into()))?;
        client.reject_enrollment(&request_id).await
    }

    // ── revocation ───────────────────────────────────────────────────────────

    /// Revoke `device_id_hex` and rotate to a fresh write epoch: post the
    /// signed transition, then publish the new keyring to every retained
    /// device and the recovery authority (owed until each lands).
    pub async fn revoke(&self, device_id_hex: &str) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        let target = Hex(device_id_hex.to_string())
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad device id".into()))?;
        self.finish_pending_membership(&client).await?;
        self.pull_membership(&client).await?;
        if !self.is_ready() {
            return Err(EngineError::Other(
                "vault is not ready for revocation".into(),
            ));
        }
        let (record, next_state, next_epoch, fresh_key) = {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("not enrolled".into()))?;
            let head = trust.head();
            if head.active_device(&trust.device_id).is_none() {
                return Err(EngineError::Other("this device is revoked".into()));
            }
            if target == trust.device_id {
                return Err(EngineError::Other(
                    "use another approved device to revoke this device".into(),
                ));
            }
            if head.active_device(&target).is_none() {
                return Err(EngineError::Other(
                    "that device is not an active member".into(),
                ));
            }
            let mut payload = head.next_payload(Operation::RevokeDevice);
            for device in payload.devices.iter_mut() {
                if device.device_id == target {
                    device.status = DeviceStatus::Revoked;
                }
            }
            let binding = policy::policy_binding(
                *head.vault_id(),
                *head.generation(),
                payload.epoch,
                trust.device_id,
                *head.hash(),
            );
            let record = policy::encode_policy(&binding, &payload, &trust.signer)
                .map_err(|e| EngineError::Other(format!("revoke: {e}")))?;
            let next = head
                .apply(&record)
                .map_err(|e| EngineError::Other(format!("revoke verify: {e}")))?;
            let mut fresh = [0u8; 32];
            zeron_crypto::fill_random(&mut fresh).map_err(|e| EngineError::Other(e.to_string()))?;
            (record, next.clone(), next.epoch(), fresh)
        };
        // Journal locally: new epoch key + owed envelopes for every retained
        // recipient. The revoked device is deliberately not a recipient.
        {
            let mut guarded = lock(&self.inner.guarded);
            if let Some(vault) = guarded.state.vault.as_mut() {
                vault.membership.push(encode_base64(&record));
            }
            let recipients: Vec<[u8; 16]> = next_state
                .devices()
                .iter()
                .filter(|d| d.status == DeviceStatus::Active)
                .map(|d| d.device_id)
                .chain(std::iter::once(next_state.recovery_authority_id()))
                .collect();
            if let Some(trust) = guarded.trust.as_mut() {
                trust
                    .keyring
                    .insert(next_epoch, &fresh_key)
                    .map_err(|e| EngineError::Other(format!("keyring: {e}")))?;
                trust.history.push(Revision {
                    hash: *next_state.hash(),
                    state: next_state.clone(),
                });
                guarded.state.keyring = Some(Secret::of(&trust.keyring.encode()));
            }
            for recipient in recipients {
                guarded
                    .state
                    .owed_envelopes
                    .push((Hex::of(&recipient), next_epoch));
            }
            guarded.state.pending_membership = Some(encode_base64(&record));
            self.commit(&mut guarded)?;
        }
        self.finish_pending_membership(&client).await
    }

    // ── recovery ─────────────────────────────────────────────────────────────

    /// Rejoin with the recovery kit and no existing device: verify the
    /// server's chain against the kit's authority, open the recovery
    /// envelope, and sign a recovery transition that admits this device
    /// under a fresh epoch. `expected_genesis` (from the recovery file) pins
    /// the vault when available.
    pub async fn recover(
        &self,
        kit_text: &str,
        expected_genesis: Option<[u8; 32]>,
    ) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        let client = self.client()?.clone();
        if lock(&self.inner.guarded).trust.is_some() {
            return Err(EngineError::Other("this device is already enrolled".into()));
        }
        let secret = RecoverySecret::from_kit(kit_text)
            .map_err(|e| EngineError::Other(format!("recovery key: {e}")))?;
        let recovery_signer = secret
            .signer()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let recovery_encryption = secret
            .encryption_key()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let Some(descriptor) = client.descriptor().await? else {
            return Err(EngineError::Other(
                "no vault exists for this account".into(),
            ));
        };
        let vault_id = descriptor
            .vault_id
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad vault id".into()))?;
        let generation = descriptor
            .generation
            .decode::<16>()
            .ok_or_else(|| EngineError::Other("bad generation".into()))?;
        let records = fetch_all_membership(&client).await?;
        let head = verify_chain(&records, &vault_id, &generation, &self.inner.profile_hash)
            .map_err(|reason| EngineError::Other(format!("remote vault rejected: {reason}")))?;
        if let Some(expected) = expected_genesis
            && *head.genesis_hash() != expected
        {
            return Err(EngineError::Other(
                "the vault on the server is not the one named in the recovery file".into(),
            ));
        }
        let authority = head.recovery_authority_id();
        if authority != *recovery_signer.author_id() {
            return Err(EngineError::Other(
                "this recovery key does not belong to the current vault (it may have been replaced)"
                    .into(),
            ));
        }
        let envelope = client
            .get_envelope(&authority)
            .await?
            .ok_or_else(|| EngineError::Other("no recovery envelope is published".into()))?;
        let history = replay_chain(&records, &vault_id, &generation, &self.inner.profile_hash)
            .map_err(EngineError::Other)?;
        let keyring = open_envelope_against(
            &history,
            &envelope,
            RecipientKind::Recovery,
            &authority,
            &recovery_encryption,
        )
        .map_err(|reason| EngineError::Other(format!("recovery envelope rejected: {reason}")))?;

        let identity = {
            let mut guarded = lock(&self.inner.guarded);
            self.ensure_identity(&mut guarded)?
        };
        let mut payload = head.next_payload(Operation::RecoveryTransition);
        payload.devices.push(identity.entry());
        let binding =
            policy::policy_binding(vault_id, generation, payload.epoch, authority, *head.hash());
        let record = policy::encode_policy(&binding, &payload, &recovery_signer)
            .map_err(|e| EngineError::Other(format!("recovery transition: {e}")))?;
        let next = head
            .apply(&record)
            .map_err(|e| EngineError::Other(format!("recovery verify: {e}")))?;
        let mut keyring = keyring;
        keyring
            .insert_fresh(next.epoch())
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let recipients: Vec<[u8; 16]> = next
            .devices()
            .iter()
            .filter(|d| d.status == DeviceStatus::Active)
            .map(|d| d.device_id)
            .chain(std::iter::once(next.recovery_authority_id()))
            .collect();
        {
            let mut records = records;
            records.push(encode_base64(&record));
            let mut guarded = lock(&self.inner.guarded);
            guarded.state.vault = Some(PinnedVault {
                vault_id: Hex::of(&vault_id),
                generation: Hex::of(&generation),
                profile_hash: Hex::of(&self.inner.profile_hash),
                membership: records,
            });
            guarded.state.keyring = Some(Secret::of(&keyring.encode()));
            guarded.state.enrollment = None;
            for recipient in recipients {
                guarded
                    .state
                    .owed_envelopes
                    .push((Hex::of(&recipient), next.epoch()));
            }
            guarded.state.pending_membership = Some(encode_base64(&record));
            self.commit(&mut guarded)?;
            guarded.trust = rebuild_trust(&guarded.state, &self.inner.profile_hash)
                .map_err(EngineError::Other)?;
            guarded.remote_vault = Some(true);
        }
        self.finish_pending_membership(&client).await
    }

    // ── content key material ─────────────────────────────────────────────────

    /// Sealing material for `object_id` under the current epoch. Creates and
    /// publishes the object's key for this epoch when none exists; the
    /// envelope is durable on the control plane before this returns.
    pub async fn seal_material(&self, object_id: [u8; 16]) -> Result<ChatKeyMaterial, EngineError> {
        let client = self.client()?.clone();
        let (epoch, cached) = {
            let guarded = lock(&self.inner.guarded);
            if guarded.content_blocked() {
                return Err(EngineError::Other(
                    "vault verification or storage is unavailable".into(),
                ));
            }
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("vault not ready".into()))?;
            let head = trust.head();
            if head.active_device(&trust.device_id).is_none() {
                return Err(EngineError::Other("this device is revoked".into()));
            }
            if trust.keyring.epoch_key(head.epoch()).is_none() {
                return Err(EngineError::Other("waiting for encryption keys".into()));
            }
            let epoch = head.epoch();
            let cached = trust.object_keys.get(&(object_id, epoch)).cloned();
            (epoch, cached)
        };
        if cached.is_none() {
            self.fetch_object_keys(&client, object_id).await?;
        }
        let cached = cached.or_else(|| {
            lock(&self.inner.guarded)
                .trust
                .as_ref()
                .and_then(|t| t.object_keys.get(&(object_id, epoch)).cloned())
        });
        let key = match cached {
            Some(key) => key,
            None => self.create_object_key(&client, object_id, epoch).await?,
        };
        let guarded = lock(&self.inner.guarded);
        let trust = guarded
            .trust
            .as_ref()
            .ok_or_else(|| EngineError::Other("vault not ready".into()))?;
        let head = trust.head();
        if head.epoch() != epoch
            || head.active_device(&trust.device_id).is_none()
            || guarded.content_blocked()
        {
            return Err(EngineError::Other("vault state changed; retry".into()));
        }
        Ok(ChatKeyMaterial {
            binding: head.content_binding(object_id, trust.device_id),
            key,
            signer: trust.signer.clone(),
        })
    }

    async fn fetch_object_keys(
        &self,
        client: &VaultClient,
        object_id: [u8; 16],
    ) -> Result<(), EngineError> {
        let entries = client.object_keys(&object_id).await?;
        let mut guarded = lock(&self.inner.guarded);
        let mut changed = false;
        {
            let Guarded { state, trust, .. } = &mut *guarded;
            let Some(trust) = trust.as_mut() else {
                return Ok(());
            };
            for entry in entries {
                let Some(bytes) = decode_base64(&entry.record) else {
                    continue;
                };
                match unwrap_object_key_against(trust, &bytes, object_id) {
                    Ok(key) => {
                        let epoch = key.scope().epoch;
                        if let std::collections::btree_map::Entry::Vacant(slot) =
                            trust.object_keys.entry((object_id, epoch))
                        {
                            cache_object_key(state, object_id, &key);
                            slot.insert(Arc::new(key));
                            changed = true;
                        }
                    }
                    Err(reason) => tracing::debug!(reason, "vault: object key envelope skipped"),
                }
            }
        }
        if changed {
            self.commit(&mut guarded)?;
        }
        Ok(())
    }

    async fn create_object_key(
        &self,
        client: &VaultClient,
        object_id: [u8; 16],
        epoch: u64,
    ) -> Result<Arc<ContentKey>, EngineError> {
        let (record, key) = {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("vault not ready".into()))?;
            let head = trust.head();
            let binding = head.envelope_binding(object_id, epoch, trust.device_id);
            let key = ContentKey::generate(KeyScope::from(&binding))
                .map_err(|e| EngineError::Other(e.to_string()))?;
            let epoch_key = trust
                .keyring
                .epoch_key(epoch)
                .ok_or_else(|| EngineError::Other("waiting for encryption keys".into()))?;
            let record = envelope::wrap_object_key(&binding, epoch_key, &key, &trust.signer)
                .map_err(|e| EngineError::Other(format!("object key: {e}")))?;
            (record.into_encoded(), key)
        };
        let published = client.put_object_key(&object_id, record).await?;
        let key = if published.adopted_existing {
            let guarded = lock(&self.inner.guarded);
            let trust = guarded
                .trust
                .as_ref()
                .ok_or_else(|| EngineError::Other("vault not ready".into()))?;
            unwrap_object_key_against(trust, &published.record, object_id).map_err(|reason| {
                EngineError::Other(format!("adopted object key rejected: {reason}"))
            })?
        } else {
            key
        };
        let key = Arc::new(key);
        let mut guarded = lock(&self.inner.guarded);
        if let Some(trust) = guarded.trust.as_mut() {
            trust.object_keys.insert((object_id, epoch), key.clone());
        }
        cache_object_key(&mut guarded.state, object_id, &key);
        self.commit(&mut guarded)?;
        Ok(key)
    }

    /// This device's Ed25519 public key (verifies its own outbox records).
    pub fn signing_public_key(&self) -> Option<[u8; 32]> {
        lock(&self.inner.guarded)
            .trust
            .as_ref()
            .map(|t| t.signing_key())
    }

    /// The binding a record sealed RIGHT NOW for `object_id` would carry.
    pub fn current_content_binding(&self, object_id: [u8; 16]) -> Option<RecordBinding> {
        let guarded = lock(&self.inner.guarded);
        if guarded.content_blocked() {
            return None;
        }
        let trust = guarded.trust.as_ref()?;
        let head = trust.head();
        head.active_device(&trust.device_id)?;
        trust.keyring.epoch_key(head.epoch())?;
        Some(head.content_binding(object_id, trust.device_id))
    }

    /// Kick a background key refresh (membership + keyring + this object's
    /// keys). Callers that hit `KeyUnavailable` on a synchronous path use
    /// this; the status watch tells them when to resume.
    pub fn spawn_key_refresh(&self, object_id: [u8; 16]) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(err) = service.refresh().await {
                tracing::debug!(error = %err, "vault: background refresh failed");
            }
            if let Ok(client) = service.client() {
                let client = client.clone();
                if let Err(err) = service.fetch_object_keys(&client, object_id).await {
                    tracing::debug!(error = %err, "vault: object key prefetch failed");
                }
            }
            service.publish_status();
        });
    }

    /// Synchronous twin of [`Self::open_material`]: answers from pinned
    /// history and cached keys only; `KeyUnavailable` means a fetch is needed.
    pub fn open_material_cached(
        &self,
        object_id: [u8; 16],
        untrusted: &RecordBinding,
    ) -> Result<OpenContext, OpenFailure> {
        let (binding, author_public_key, have_epoch, cached) =
            self.trusted_context(object_id, untrusted)?;
        if !have_epoch {
            return Err(OpenFailure::KeyUnavailable);
        }
        let key = cached.ok_or(OpenFailure::KeyUnavailable)?;
        Ok(OpenContext {
            binding,
            key,
            author_public_key,
        })
    }

    fn trusted_context(
        &self,
        object_id: [u8; 16],
        untrusted: &RecordBinding,
    ) -> Result<TrustedContext, OpenFailure> {
        if untrusted.kind != RecordKind::Content || untrusted.object_id != object_id {
            return Err(OpenFailure::NotAuthorized);
        }
        let guarded = lock(&self.inner.guarded);
        if guarded.content_blocked() {
            return Err(OpenFailure::Unavailable);
        }
        let trust = guarded.trust.as_ref().ok_or(OpenFailure::Unavailable)?;
        let revision = trust
            .revision(&untrusted.membership_hash)
            .ok_or(OpenFailure::NotAuthorized)?;
        let state = &revision.state;
        if state.epoch() != untrusted.epoch
            || *state.vault_id() != untrusted.vault_id
            || *state.generation() != untrusted.generation
        {
            return Err(OpenFailure::NotAuthorized);
        }
        let author = state
            .active_device(&untrusted.author_id)
            .ok_or(OpenFailure::NotAuthorized)?;
        Ok((
            state.content_binding(object_id, author.device_id),
            author.signing_key,
            trust.keyring.epoch_key(untrusted.epoch).is_some(),
            trust
                .object_keys
                .get(&(object_id, untrusted.epoch))
                .cloned(),
        ))
    }

    /// Opening material for a record whose UNTRUSTED binding was parsed
    /// from the wire. The returned binding is rebuilt from pinned history;
    /// callers verify the record against it, never against the parsed one.
    pub async fn open_material(
        &self,
        object_id: [u8; 16],
        untrusted: &RecordBinding,
    ) -> Result<OpenContext, OpenFailure> {
        let trusted = self.trusted_context(object_id, untrusted)?;
        let (binding, author_public_key, have_epoch, cached) = trusted;
        if !have_epoch {
            return Err(OpenFailure::KeyUnavailable);
        }
        let key = match cached {
            Some(key) => key,
            None => {
                let client = self.client().map_err(|_| OpenFailure::Unavailable)?.clone();
                self.fetch_object_keys(&client, object_id)
                    .await
                    .map_err(|_| OpenFailure::KeyUnavailable)?;
                lock(&self.inner.guarded)
                    .trust
                    .as_ref()
                    .and_then(|t| t.object_keys.get(&(object_id, untrusted.epoch)).cloned())
                    .ok_or(OpenFailure::KeyUnavailable)?
            }
        };
        Ok(OpenContext {
            binding,
            key,
            author_public_key,
        })
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

struct Identity {
    device_id: [u8; 16],
    signer: Arc<DeviceSigner>,
    encryption: HpkePrivateKey,
}

impl Identity {
    fn signing_key(&self) -> [u8; 32] {
        self.signer.public_key().try_into().expect("32-byte key")
    }

    fn entry(&self) -> policy::DeviceEntry {
        policy::DeviceEntry {
            device_id: self.device_id,
            signing_key: self.signing_key(),
            encryption_key: *self.encryption.public_key().as_bytes(),
            status: DeviceStatus::Active,
        }
    }
}

fn identity_of(state: &LocalVaultState) -> Result<Identity, EngineError> {
    let device = state
        .device
        .as_ref()
        .ok_or_else(|| EngineError::Other("no device identity".into()))?;
    let device_id = device
        .device_id
        .decode::<16>()
        .ok_or_else(|| EngineError::Other("bad device id".into()))?;
    let seed = device
        .signing_seed
        .decode::<32>()
        .ok_or_else(|| EngineError::Other("bad signing seed".into()))?;
    let encryption = device
        .encryption_secret
        .decode::<32>()
        .ok_or_else(|| EngineError::Other("bad encryption secret".into()))?;
    Ok(Identity {
        device_id,
        signer: Arc::new(
            DeviceSigner::from_seed(device_id, &seed)
                .map_err(|e| EngineError::Other(e.to_string()))?,
        ),
        encryption: HpkePrivateKey::from_bytes(&encryption)
            .map_err(|e| EngineError::Other(e.to_string()))?,
    })
}

fn replay_chain(
    records: &[String],
    vault_id: &[u8; 16],
    generation: &[u8; 16],
    profile_hash: &[u8; 32],
) -> Result<Vec<Revision>, String> {
    if records.len() > MAX_MEMBERSHIP_RECORDS {
        return Err("membership history too long".into());
    }
    let mut history = Vec::with_capacity(records.len());
    for (index, encoded) in records.iter().enumerate() {
        let bytes = decode_base64(encoded).ok_or("malformed membership record")?;
        let state = if index == 0 {
            MembershipState::from_genesis(&bytes, vault_id, generation, profile_hash)
                .map_err(|e| format!("genesis: {e}"))?
        } else {
            let previous: &Revision = history.last().expect("genesis present");
            previous
                .state
                .apply(&bytes)
                .map_err(|e| format!("record {index}: {e}"))?
        };
        history.push(Revision {
            hash: *state.hash(),
            state,
        });
    }
    if history.is_empty() {
        return Err("empty membership".into());
    }
    Ok(history)
}

fn verify_chain(
    records: &[String],
    vault_id: &[u8; 16],
    generation: &[u8; 16],
    profile_hash: &[u8; 32],
) -> Result<MembershipState, String> {
    replay_chain(records, vault_id, generation, profile_hash)
        .map(|h| h.last().expect("non-empty").state.clone())
}

fn rebuild_trust(
    state: &LocalVaultState,
    profile_hash: &[u8; 32],
) -> Result<Option<Trust>, String> {
    let Some(vault) = state.vault.as_ref() else {
        return Ok(None);
    };
    // A pinned vault without a keyring is a pending enrollment, not trust.
    let Some(keyring_bytes) = state.keyring.as_ref() else {
        return Ok(None);
    };
    let identity = identity_of(state).map_err(|e| e.to_string())?;
    let vault_id = vault.vault_id.decode::<16>().ok_or("bad vault id")?;
    let generation = vault.generation.decode::<16>().ok_or("bad generation")?;
    if vault.profile_hash.decode::<32>() != Some(*profile_hash) {
        return Err("vault state belongs to another profile".into());
    }
    let history = replay_chain(&vault.membership, &vault_id, &generation, profile_hash)?;
    let keyring = Keyring::decode(&keyring_bytes.bytes().ok_or("bad keyring")?)
        .map_err(|e| format!("keyring: {e}"))?;
    let mut object_keys = BTreeMap::new();
    for (object_hex, entries) in &state.object_keys {
        let Some(object_id) = Hex(object_hex.clone()).decode::<16>() else {
            continue;
        };
        for entry in entries {
            let scope = KeyScope {
                vault_id,
                generation,
                epoch: entry.epoch,
                object_id,
            };
            if let (Some(id), Some(bytes)) = (entry.key_id.decode::<16>(), entry.key.bytes())
                && let Ok(key) = ContentKey::from_bytes(scope, id, &bytes)
            {
                object_keys.insert((object_id, entry.epoch), Arc::new(key));
            }
        }
    }
    Ok(Some(Trust {
        device_id: identity.device_id,
        signer: identity.signer,
        encryption: identity.encryption,
        history,
        keyring,
        object_keys,
    }))
}

fn cache_object_key(state: &mut LocalVaultState, object_id: [u8; 16], key: &ContentKey) {
    let entries = state.object_keys.entry(Hex::of(&object_id).0).or_default();
    let epoch = key.scope().epoch;
    if entries.iter().any(|e| e.epoch == epoch) {
        return;
    }
    entries.push(CachedObjectKey {
        epoch,
        key_id: Hex::of(key.identifier()),
        key: Secret::of(key.expose_secret()),
    });
}

/// Verify an envelope record against pinned history: its membership hash
/// must name a revision at which its author was active.
fn envelope_context(
    history: &[Revision],
    encoded: &[u8],
    object_id: &[u8; 16],
) -> Result<(RecordBinding, [u8; 32]), String> {
    let parsed = UnverifiedRecord::parse(encoded, envelope::MAX_ENVELOPE_PAYLOAD_BYTES)
        .map_err(|e| format!("parse: {e}"))?;
    let untrusted = *parsed.untrusted_binding();
    let revision = history
        .iter()
        .rev()
        .find(|r| r.hash == untrusted.membership_hash)
        .ok_or("unknown membership hash")?;
    let state = &revision.state;
    let author = state
        .active_device(&untrusted.author_id)
        .ok_or("author not active at that revision")?;
    if untrusted.epoch > state.epoch() || untrusted.object_id != *object_id {
        return Err("envelope context mismatch".into());
    }
    Ok((
        state.envelope_binding(*object_id, untrusted.epoch, author.device_id),
        author.signing_key,
    ))
}

fn open_envelope_against(
    history: &[Revision],
    encoded: &[u8],
    kind: RecipientKind,
    recipient: &[u8; 16],
    recipient_key: &HpkePrivateKey,
) -> Result<Keyring, String> {
    let (binding, author_key) = envelope_context(history, encoded, &POLICY_OBJECT_ID)?;
    envelope::open_keyring(
        encoded,
        &binding,
        kind,
        recipient,
        recipient_key,
        &author_key,
    )
    .map_err(|e| format!("open: {e}"))
}

fn open_keyring_envelope(
    trust: &Trust,
    encoded: &[u8],
    kind: RecipientKind,
    recipient: &[u8; 16],
) -> Result<Keyring, String> {
    open_envelope_against(&trust.history, encoded, kind, recipient, &trust.encryption)
}

fn unwrap_object_key_against(
    trust: &Trust,
    encoded: &[u8],
    object_id: [u8; 16],
) -> Result<ContentKey, String> {
    let (binding, author_key) = envelope_context(&trust.history, encoded, &object_id)?;
    let epoch_key = trust
        .keyring
        .epoch_key(binding.epoch)
        .ok_or("epoch key not held")?;
    envelope::unwrap_object_key(encoded, &binding, epoch_key, &author_key)
        .map_err(|e| format!("unwrap: {e}"))
}

fn seal_keyring_for(
    trust: &Trust,
    recipient: &[u8; 16],
    epoch: u64,
) -> Option<Result<Vec<u8>, String>> {
    let head = trust.head();
    if head.active_device(&trust.device_id).is_none()
        || trust.keyring.epoch_key(head.epoch()).is_none()
    {
        return Some(Err(
            "current membership and epoch key required for envelopes".into(),
        ));
    }
    let (kind, public) = if *recipient == head.recovery_authority_id() {
        (RecipientKind::Recovery, *head.recovery_encryption_key())
    } else {
        let device = head.active_device(recipient)?;
        (RecipientKind::Device, device.encryption_key)
    };
    let Ok(public) = HpkePublicKey::from_bytes(&public) else {
        return Some(Err("bad recipient key".into()));
    };
    Some(
        envelope::seal_keyring(
            &head.envelope_binding(POLICY_OBJECT_ID, epoch.max(head.epoch()), trust.device_id),
            kind,
            recipient,
            &public,
            &trust.keyring,
            &trust.signer,
        )
        .map(|sealed| sealed.into_encoded())
        .map_err(|e| format!("seal: {e}")),
    )
}

async fn fetch_all_membership(client: &VaultClient) -> Result<Vec<String>, EngineError> {
    let mut records = Vec::new();
    let mut after = -1;
    loop {
        let page = client.membership_after(after).await?;
        let got = page.records.len() as i64;
        records.extend(page.records);
        if !page.truncated || got == 0 {
            break;
        }
        after += got;
        if records.len() > MAX_MEMBERSHIP_RECORDS {
            return Err(EngineError::Other("membership history too long".into()));
        }
    }
    Ok(records)
}

fn request_from_info(
    info: &super::client::EnrollmentInfo,
    vault_id: [u8; 16],
) -> Option<EnrollmentRequest> {
    Some(EnrollmentRequest {
        vault_id,
        request_id: info.request_id.decode::<16>()?,
        device_id: info.device_id.decode::<16>()?,
        signing_key: info.signing_key.decode::<32>()?,
        encryption_key: info.encryption_key.decode::<32>()?,
    })
}

fn pending_request(state: &LocalVaultState, request_id: &Hex) -> Option<EnrollmentRequest> {
    let identity = identity_of(state).ok()?;
    let vault = state.vault.as_ref()?;
    Some(EnrollmentRequest {
        vault_id: vault.vault_id.decode::<16>()?,
        request_id: request_id.decode::<16>()?,
        device_id: identity.device_id,
        signing_key: identity.signing_key(),
        encryption_key: *identity.encryption.public_key().as_bytes(),
    })
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn recovery_file(state: &MembershipState) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "kind": "zeron-recovery-file",
        "vaultId": Hex::of(state.vault_id()).0,
        "generation": Hex::of(state.generation()).0,
        "genesisHash": Hex::of(state.genesis_hash()).0,
        "profileHash": Hex::of(state.profile_hash()).0,
        "recoveryAuthorityId": Hex::of(&state.recovery_authority_id()).0,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::store::MemoryProtection;
    use super::*;
    use crate::doc_host::EdgeConfig;

    fn client() -> VaultClient {
        VaultClient::new(
            reqwest::Client::new(),
            EdgeConfig::with_static_token("http://127.0.0.1:1", "test"),
            "org",
        )
    }

    #[test]
    fn locked_and_corrupt_stores_never_allow_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        store.save(&LocalVaultState::default()).unwrap();
        let original = std::fs::read(dir.path().join("vault.json")).unwrap();
        let reopened = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        let vault = VaultService::open(reopened, Some(client()), "org", "user");
        assert!(matches!(vault.status().phase, VaultPhase::Locked { .. }));
        assert!(vault.is_enrolled());
        assert_eq!(
            std::fs::read(dir.path().join("vault.json")).unwrap(),
            original
        );
        std::fs::write(dir.path().join("vault.json"), b"invalid").unwrap();
        let reopened = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        assert!(VaultService::open(reopened, Some(client()), "org", "user").is_enrolled());
    }

    #[tokio::test]
    async fn relay_stream_closes_when_encryption_becomes_required() {
        use futures::StreamExt;
        use zeron_rpc::{RpcReply, RpcService};
        struct Streaming;
        #[async_trait::async_trait]
        impl RpcService for Streaming {
            async fn handle(
                &self,
                _: &str,
                _: serde_json::Value,
            ) -> Result<RpcReply, zeron_rpc::RpcError> {
                Ok(RpcReply::Stream(futures::stream::pending().boxed()))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        let vault = VaultService::open(store, Some(client()), "org", "user");
        let relay = crate::rpc::RelayRpc::new(Arc::new(Streaming), vault.clone());
        let RpcReply::Stream(mut stream) =
            relay.handle("Watch", serde_json::json!({})).await.unwrap()
        else {
            panic!("expected stream");
        };
        lock(&vault.inner.guarded).encryption_required = true;
        vault.publish_status();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn remote_vault_requires_encryption_before_device_approval() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::new(dir.path(), "org/user", Box::new(MemoryProtection::new()));
        let vault = VaultService::open(store, Some(client()), "org", "user");
        assert!(!vault.is_enrolled());
        lock(&vault.inner.guarded).remote_vault = Some(true);
        assert!(vault.is_enrolled());
        assert!(!vault.is_ready());
    }
}
