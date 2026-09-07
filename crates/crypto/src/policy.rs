//! Signed membership policy records (RFC 0001 §5, §11): the genesis record
//! and the parent-linked history of device additions, revocations, recovery
//! authority changes, and write-epoch transitions. A verified
//! [`MembershipState`] is the ONLY source of "who may sign for this vault";
//! every content/envelope verification takes its expected binding and the
//! author's public key from here, never from the record being checked.
//!
//! Wire form: the signed wrapper (`record.rs`, kind = Policy) whose payload
//! is one deterministic CBOR map:
//!
//! ```text
//!   0  policy version          unsigned, exactly 1
//!   1  sequence                unsigned; genesis = 0, then parent + 1
//!   2  parent membership hash  32 bytes; genesis = zeros
//!   3  profile hash            32 bytes (SHA-256 of the org/user labels)
//!   4  active write epoch      unsigned ≥ 1
//!   5  operation               1 genesis, 2 add device, 3 revoke device,
//!                              4 rotate recovery, 5 recovery transition
//!   6  recovery signing key    32 bytes (Ed25519)
//!   7  recovery encryption key 32 bytes (X25519)
//!   8  devices                 array of [id 16, signing 32, encryption 32,
//!                              status unsigned (0 active, 1 revoked)]
//! ```
//!
//! The membership hash of a record is SHA-256("zeron/membership/v1" || 0x00
//! || complete signed record bytes). Wrapper fields for policy records:
//! object ID = [`POLICY_OBJECT_ID`], epoch = the record's active epoch,
//! membership hash = the PARENT's hash (genesis: zeros).

use crate::content::DeviceSigner;
use crate::record::{self, Reader, RecordBinding, RecordError, RecordKind, UnverifiedRecord};
use crate::{CryptoError, sha256};
use std::fmt;

pub const POLICY_OBJECT_ID: [u8; 16] = [0; 16];
pub const MAX_POLICY_BYTES: usize = 64 * 1024;
pub const MAX_DEVICES: usize = 64;
const MEMBERSHIP_DOMAIN: &[u8] = b"zeron/membership/v1\0";
const PROFILE_DOMAIN: &[u8] = b"zeron/profile/v1\0";
const RECOVERY_ID_DOMAIN: &[u8] = b"zeron/recovery-id/v1\0";
const ENROLL_DOMAIN: &[u8] = b"zeron/enroll/v1\0";
const PAIRING_DOMAIN: &[u8] = b"zeron/pairing-code/v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    Record(RecordError),
    Crypto(CryptoError),
    Malformed,
    UnsupportedVersion,
    UnsupportedOperation,
    WrongVault,
    WrongProfile,
    WrongSequence,
    WrongParent,
    WrongEpoch,
    UnknownAuthor,
    RevokedAuthor,
    InvalidDeviceSet,
    InvalidRecoveryKeys,
    TooManyDevices,
    InvalidTransition,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PolicyError {}
impl From<RecordError> for PolicyError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
impl From<CryptoError> for PolicyError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}
impl From<crate::content::ContentError> for PolicyError {
    fn from(_: crate::content::ContentError) -> Self {
        Self::Malformed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum DeviceStatus {
    Active = 0,
    Revoked = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Operation {
    Genesis = 1,
    AddDevice = 2,
    RevokeDevice = 3,
    RotateRecovery = 4,
    RecoveryTransition = 5,
}

impl TryFrom<u64> for Operation {
    type Error = PolicyError;
    fn try_from(value: u64) -> Result<Self, PolicyError> {
        match value {
            1 => Ok(Self::Genesis),
            2 => Ok(Self::AddDevice),
            3 => Ok(Self::RevokeDevice),
            4 => Ok(Self::RotateRecovery),
            5 => Ok(Self::RecoveryTransition),
            _ => Err(PolicyError::UnsupportedOperation),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub device_id: [u8; 16],
    pub signing_key: [u8; 32],
    pub encryption_key: [u8; 32],
    pub status: DeviceStatus,
}

/// The decoded policy payload. Public because the control plane and native
/// clients build these; only [`MembershipState`] decides whether one is valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyPayload {
    pub sequence: u64,
    pub parent_hash: [u8; 32],
    pub profile_hash: [u8; 32],
    pub epoch: u64,
    pub operation: Operation,
    pub recovery_signing_key: [u8; 32],
    pub recovery_encryption_key: [u8; 32],
    pub devices: Vec<DeviceEntry>,
}

impl PolicyPayload {
    pub fn encode(&self) -> Result<Vec<u8>, PolicyError> {
        if self.devices.len() > MAX_DEVICES {
            return Err(PolicyError::TooManyDevices);
        }
        let mut out = Vec::with_capacity(256 + self.devices.len() * 96);
        record::argument(&mut out, 5, 9);
        record::uint_field(&mut out, 0, 1);
        record::uint_field(&mut out, 1, self.sequence);
        record::bytes_field(&mut out, 2, &self.parent_hash);
        record::bytes_field(&mut out, 3, &self.profile_hash);
        record::uint_field(&mut out, 4, self.epoch);
        record::uint_field(&mut out, 5, self.operation as u64);
        record::bytes_field(&mut out, 6, &self.recovery_signing_key);
        record::bytes_field(&mut out, 7, &self.recovery_encryption_key);
        record::argument(&mut out, 0, 8);
        record::argument(&mut out, 4, self.devices.len() as u64);
        for device in &self.devices {
            record::argument(&mut out, 4, 4);
            record::argument(&mut out, 2, 16);
            out.extend_from_slice(&device.device_id);
            record::argument(&mut out, 2, 32);
            out.extend_from_slice(&device.signing_key);
            record::argument(&mut out, 2, 32);
            out.extend_from_slice(&device.encryption_key);
            record::argument(&mut out, 0, device.status as u64);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PolicyError> {
        let mut reader = Reader::new(bytes);
        if reader.argument(5)? != 9 {
            return Err(PolicyError::Malformed);
        }
        if reader.uint_field(0)? != 1 {
            return Err(PolicyError::UnsupportedVersion);
        }
        let sequence = reader.uint_field(1)?;
        let parent_hash = reader.fixed_field(2)?;
        let profile_hash = reader.fixed_field(3)?;
        let epoch = reader.uint_field(4)?;
        let operation = Operation::try_from(reader.uint_field(5)?)?;
        let recovery_signing_key = reader.fixed_field(6)?;
        let recovery_encryption_key = reader.fixed_field(7)?;
        if reader.argument(0)? != 8 {
            return Err(PolicyError::Malformed);
        }
        let count = reader.argument(4)?;
        if count > MAX_DEVICES as u64 {
            return Err(PolicyError::TooManyDevices);
        }
        let mut devices = Vec::with_capacity(count as usize);
        for _ in 0..count {
            if reader.argument(4)? != 4 {
                return Err(PolicyError::Malformed);
            }
            let device_id = reader.fixed_bytes::<16>()?;
            let signing_key = reader.fixed_bytes::<32>()?;
            let encryption_key = reader.fixed_bytes::<32>()?;
            let status = match reader.argument(0)? {
                0 => DeviceStatus::Active,
                1 => DeviceStatus::Revoked,
                _ => return Err(PolicyError::Malformed),
            };
            devices.push(DeviceEntry {
                device_id,
                signing_key,
                encryption_key,
                status,
            });
        }
        reader.finish()?;
        Ok(Self {
            sequence,
            parent_hash,
            profile_hash,
            epoch,
            operation,
            recovery_signing_key,
            recovery_encryption_key,
            devices,
        })
    }
}

/// SHA-256 binding of the account profile without publishing its labels.
pub fn profile_hash(org_id: &str, user_id: &str) -> [u8; 32] {
    sha256(&[PROFILE_DOMAIN, org_id.as_bytes(), b"\0", user_id.as_bytes()])
}

/// Membership hash of a complete signed policy record.
pub fn membership_hash(encoded_record: &[u8]) -> [u8; 32] {
    sha256(&[MEMBERSHIP_DOMAIN, encoded_record])
}

/// The author ID under which the recovery authority signs (RFC §5): derived
/// from its public signing key so it needs no separate registration.
pub fn recovery_authority_id(recovery_signing_key: &[u8; 32]) -> [u8; 16] {
    let digest = sha256(&[RECOVERY_ID_DOMAIN, recovery_signing_key]);
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

/// An enrollment request: the pending device's public identity, bound to
/// the vault it wants to join. The proof is an ordinary Ed25519 signature by
/// the device's signing key over [`enrollment_proof_input`]; it proves key
/// possession to the bootstrap service, never membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub vault_id: [u8; 16],
    pub request_id: [u8; 16],
    pub device_id: [u8; 16],
    pub signing_key: [u8; 32],
    pub encryption_key: [u8; 32],
}

impl EnrollmentRequest {
    pub fn proof_input(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENROLL_DOMAIN.len() + 112);
        out.extend_from_slice(ENROLL_DOMAIN);
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.request_id);
        out.extend_from_slice(&self.device_id);
        out.extend_from_slice(&self.signing_key);
        out.extend_from_slice(&self.encryption_key);
        out
    }

    pub fn sign(&self, signer: &DeviceSigner) -> Result<[u8; 64], PolicyError> {
        if *signer.author_id() != self.device_id
            || signer.public_key() != self.signing_key.as_slice()
        {
            return Err(PolicyError::UnknownAuthor);
        }
        Ok(signer.sign_bytes(&self.proof_input())?)
    }

    pub fn verify(&self, proof: &[u8]) -> Result<(), PolicyError> {
        if !crate::ed25519_point_encoding_precheck(&self.signing_key)
            || self.encryption_key.iter().all(|byte| *byte == 0)
            || self.device_id == POLICY_OBJECT_ID
        {
            return Err(PolicyError::InvalidDeviceSet);
        }
        crate::verify_ed25519(&self.signing_key, &self.proof_input(), proof)?;
        Ok(())
    }

    /// The human-comparison code (RFC §6.2): both the pending device and the
    /// approving device derive it from the request they each hold plus the
    /// genesis hash of the vault they each see, so a relay that substitutes
    /// keys OR presents a different vault produces a mismatch the user can
    /// see. Eight decimal digits as "NNNN-NNNN".
    pub fn pairing_code(&self, genesis_hash: &[u8; 32]) -> String {
        let digest = sha256(&[
            PAIRING_DOMAIN,
            &self.proof_input()[ENROLL_DOMAIN.len()..],
            genesis_hash,
        ]);
        let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 100_000_000;
        format!("{:04}-{:04}", value / 10_000, value % 10_000)
    }

    pub fn device_entry(&self) -> DeviceEntry {
        DeviceEntry {
            device_id: self.device_id,
            signing_key: self.signing_key,
            encryption_key: self.encryption_key,
            status: DeviceStatus::Active,
        }
    }
}

/// Wrapper binding for a policy record at `epoch` authored by `author_id`
/// under parent hash `parent`.
pub fn policy_binding(
    vault_id: [u8; 16],
    generation: [u8; 16],
    epoch: u64,
    author_id: [u8; 16],
    parent: [u8; 32],
) -> RecordBinding {
    RecordBinding {
        kind: RecordKind::Policy,
        vault_id,
        generation,
        epoch,
        object_id: POLICY_OBJECT_ID,
        author_id,
        membership_hash: parent,
    }
}

/// Sign and encode a policy record with a fresh revision ID. The caller is
/// responsible for the payload's meaning; validity is decided on apply.
pub fn encode_policy(
    binding: &RecordBinding,
    payload: &PolicyPayload,
    signer: &DeviceSigner,
) -> Result<Vec<u8>, PolicyError> {
    if binding.kind != RecordKind::Policy || binding.author_id != *signer.author_id() {
        return Err(PolicyError::UnknownAuthor);
    }
    let payload = payload.encode()?;
    let mut revision_id = [0; 16];
    crate::fill_random(&mut revision_id)?;
    let input = record::signing_bytes(binding, &revision_id, &payload, MAX_POLICY_BYTES)?;
    let signature = signer.sign_bytes(&input)?;
    Ok(record::encode_signed(
        binding,
        &revision_id,
        &payload,
        &signature,
        MAX_POLICY_BYTES,
    )?)
}

/// A verified membership head: the trust anchor for every other record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipState {
    vault_id: [u8; 16],
    generation: [u8; 16],
    genesis_hash: [u8; 32],
    hash: [u8; 32],
    sequence: u64,
    epoch: u64,
    profile_hash: [u8; 32],
    recovery_signing_key: [u8; 32],
    recovery_encryption_key: [u8; 32],
    devices: Vec<DeviceEntry>,
}

impl MembershipState {
    /// Pin a genesis record. `expected_*` come from local trusted state (the
    /// creating device, a pairing transcript, or the recovery kit), never from
    /// the server's descriptor alone.
    pub fn from_genesis(
        encoded: &[u8],
        expected_vault_id: &[u8; 16],
        expected_generation: &[u8; 16],
        expected_profile_hash: &[u8; 32],
    ) -> Result<Self, PolicyError> {
        let parsed = UnverifiedRecord::parse(encoded, MAX_POLICY_BYTES)?;
        let untrusted = *parsed.untrusted_binding();
        if untrusted.kind != RecordKind::Policy
            || untrusted.vault_id != *expected_vault_id
            || untrusted.generation != *expected_generation
            || untrusted.object_id != POLICY_OBJECT_ID
        {
            return Err(PolicyError::WrongVault);
        }
        let payload = PolicyPayload::decode(payload_of(encoded)?)?;
        if payload.operation != Operation::Genesis {
            return Err(PolicyError::InvalidTransition);
        }
        if payload.sequence != 0 || payload.parent_hash != [0; 32] {
            return Err(PolicyError::WrongSequence);
        }
        if payload.profile_hash != *expected_profile_hash {
            return Err(PolicyError::WrongProfile);
        }
        if payload.epoch != 1 {
            return Err(PolicyError::WrongEpoch);
        }
        check_device_entries(&payload.devices)?;
        check_recovery_keys(&payload)?;
        let [device] = payload.devices.as_slice() else {
            return Err(PolicyError::InvalidDeviceSet);
        };
        if device.status != DeviceStatus::Active {
            return Err(PolicyError::InvalidDeviceSet);
        }
        let expected = policy_binding(
            *expected_vault_id,
            *expected_generation,
            1,
            device.device_id,
            [0; 32],
        );
        parsed.verify(&expected, &device.signing_key)?;
        Ok(Self {
            vault_id: *expected_vault_id,
            generation: *expected_generation,
            genesis_hash: membership_hash(encoded),
            hash: membership_hash(encoded),
            sequence: 0,
            epoch: 1,
            profile_hash: payload.profile_hash,
            recovery_signing_key: payload.recovery_signing_key,
            recovery_encryption_key: payload.recovery_encryption_key,
            devices: payload.devices,
        })
    }

    /// Verify and apply the next record in the history.
    pub fn apply(&self, encoded: &[u8]) -> Result<Self, PolicyError> {
        let parsed = UnverifiedRecord::parse(encoded, MAX_POLICY_BYTES)?;
        let untrusted = *parsed.untrusted_binding();
        if untrusted.kind != RecordKind::Policy
            || untrusted.vault_id != self.vault_id
            || untrusted.generation != self.generation
            || untrusted.object_id != POLICY_OBJECT_ID
        {
            return Err(PolicyError::WrongVault);
        }
        if untrusted.membership_hash != self.hash {
            return Err(PolicyError::WrongParent);
        }
        let payload = PolicyPayload::decode(payload_of(encoded)?)?;
        if payload.sequence
            != self
                .sequence
                .checked_add(1)
                .ok_or(PolicyError::WrongSequence)?
        {
            return Err(PolicyError::WrongSequence);
        }
        if payload.parent_hash != self.hash {
            return Err(PolicyError::WrongParent);
        }
        if payload.profile_hash != self.profile_hash {
            return Err(PolicyError::WrongProfile);
        }
        check_device_entries(&payload.devices)?;
        check_recovery_keys(&payload)?;
        let next_epoch = self.epoch.checked_add(1).ok_or(PolicyError::WrongEpoch)?;
        let signing_key = match payload.operation {
            Operation::Genesis => return Err(PolicyError::InvalidTransition),
            Operation::RecoveryTransition => {
                if untrusted.author_id != recovery_authority_id(&self.recovery_signing_key) {
                    return Err(PolicyError::UnknownAuthor);
                }
                self.recovery_signing_key
            }
            _ => match self.device(&untrusted.author_id) {
                Some(device) if device.status == DeviceStatus::Active => device.signing_key,
                Some(_) => return Err(PolicyError::RevokedAuthor),
                None => return Err(PolicyError::UnknownAuthor),
            },
        };
        let expected_epoch = match payload.operation {
            Operation::AddDevice => self.epoch,
            _ => next_epoch,
        };
        if payload.epoch != expected_epoch || untrusted.epoch != expected_epoch {
            return Err(PolicyError::WrongEpoch);
        }
        self.check_transition(&payload)?;
        let expected = policy_binding(
            self.vault_id,
            self.generation,
            expected_epoch,
            untrusted.author_id,
            self.hash,
        );
        parsed.verify(&expected, &signing_key)?;
        Ok(Self {
            vault_id: self.vault_id,
            generation: self.generation,
            genesis_hash: self.genesis_hash,
            hash: membership_hash(encoded),
            sequence: payload.sequence,
            epoch: payload.epoch,
            profile_hash: self.profile_hash,
            recovery_signing_key: payload.recovery_signing_key,
            recovery_encryption_key: payload.recovery_encryption_key,
            devices: payload.devices,
        })
    }

    fn check_transition(&self, payload: &PolicyPayload) -> Result<(), PolicyError> {
        let recovery_unchanged = payload.recovery_signing_key == self.recovery_signing_key
            && payload.recovery_encryption_key == self.recovery_encryption_key;
        let recovery_replaced = payload.recovery_signing_key != self.recovery_signing_key
            && payload.recovery_encryption_key != self.recovery_encryption_key;
        match payload.operation {
            Operation::Genesis => Err(PolicyError::InvalidTransition),
            Operation::AddDevice => {
                if !recovery_unchanged {
                    return Err(PolicyError::InvalidRecoveryKeys);
                }
                self.expect_prefix(payload, |previous, next| previous == next)?;
                match &payload.devices[self.devices.len()..] {
                    [added] if added.status == DeviceStatus::Active => Ok(()),
                    _ => Err(PolicyError::InvalidDeviceSet),
                }
            }
            Operation::RevokeDevice => {
                if !recovery_unchanged {
                    return Err(PolicyError::InvalidRecoveryKeys);
                }
                if payload.devices.len() != self.devices.len() {
                    return Err(PolicyError::InvalidDeviceSet);
                }
                let mut revoked = 0;
                self.expect_prefix(payload, |previous, next| {
                    if previous == next {
                        return true;
                    }
                    let keys_match = previous.device_id == next.device_id
                        && previous.signing_key == next.signing_key
                        && previous.encryption_key == next.encryption_key;
                    let newly_revoked = previous.status == DeviceStatus::Active
                        && next.status == DeviceStatus::Revoked;
                    if keys_match && newly_revoked {
                        revoked += 1;
                        true
                    } else {
                        false
                    }
                })?;
                if revoked == 1 {
                    Ok(())
                } else {
                    Err(PolicyError::InvalidDeviceSet)
                }
            }
            Operation::RotateRecovery => {
                if !recovery_replaced {
                    return Err(PolicyError::InvalidRecoveryKeys);
                }
                if payload.devices != self.devices {
                    return Err(PolicyError::InvalidDeviceSet);
                }
                Ok(())
            }
            Operation::RecoveryTransition => {
                if !(recovery_unchanged || recovery_replaced) {
                    return Err(PolicyError::InvalidRecoveryKeys);
                }
                self.expect_prefix(payload, |previous, next| {
                    previous == next
                        || (previous.device_id == next.device_id
                            && previous.signing_key == next.signing_key
                            && previous.encryption_key == next.encryption_key
                            && next.status == DeviceStatus::Revoked)
                })?;
                match &payload.devices[self.devices.len()..] {
                    [added] if added.status == DeviceStatus::Active => Ok(()),
                    _ => Err(PolicyError::InvalidDeviceSet),
                }
            }
        }
    }

    /// Every existing device must appear, in order, at the same index.
    fn expect_prefix(
        &self,
        payload: &PolicyPayload,
        mut accept: impl FnMut(&DeviceEntry, &DeviceEntry) -> bool,
    ) -> Result<(), PolicyError> {
        if payload.devices.len() < self.devices.len() {
            return Err(PolicyError::InvalidDeviceSet);
        }
        for (previous, next) in self.devices.iter().zip(&payload.devices) {
            if !accept(previous, next) {
                return Err(PolicyError::InvalidDeviceSet);
            }
        }
        Ok(())
    }

    pub fn vault_id(&self) -> &[u8; 16] {
        &self.vault_id
    }
    pub fn generation(&self) -> &[u8; 16] {
        &self.generation
    }
    pub fn genesis_hash(&self) -> &[u8; 32] {
        &self.genesis_hash
    }
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn profile_hash(&self) -> &[u8; 32] {
        &self.profile_hash
    }
    pub fn recovery_signing_key(&self) -> &[u8; 32] {
        &self.recovery_signing_key
    }
    pub fn recovery_encryption_key(&self) -> &[u8; 32] {
        &self.recovery_encryption_key
    }
    pub fn recovery_authority_id(&self) -> [u8; 16] {
        recovery_authority_id(&self.recovery_signing_key)
    }
    pub fn devices(&self) -> &[DeviceEntry] {
        &self.devices
    }
    pub fn device(&self, device_id: &[u8; 16]) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|device| device.device_id == *device_id)
    }
    pub fn active_device(&self, device_id: &[u8; 16]) -> Option<&DeviceEntry> {
        self.device(device_id)
            .filter(|device| device.status == DeviceStatus::Active)
    }

    /// The payload a successor record must carry before its own changes.
    pub fn next_payload(&self, operation: Operation) -> PolicyPayload {
        PolicyPayload {
            sequence: self.sequence.saturating_add(1),
            parent_hash: self.hash,
            profile_hash: self.profile_hash,
            epoch: match operation {
                Operation::AddDevice | Operation::Genesis => self.epoch,
                _ => self.epoch.saturating_add(1),
            },
            operation,
            recovery_signing_key: self.recovery_signing_key,
            recovery_encryption_key: self.recovery_encryption_key,
            devices: self.devices.clone(),
        }
    }

    /// Binding for a content record written to `object_id` by `author_id`
    /// under the current epoch and this membership.
    pub fn content_binding(&self, object_id: [u8; 16], author_id: [u8; 16]) -> RecordBinding {
        RecordBinding {
            kind: RecordKind::Content,
            vault_id: self.vault_id,
            generation: self.generation,
            epoch: self.epoch,
            object_id,
            author_id,
            membership_hash: self.hash,
        }
    }

    /// Binding for a key envelope authored by `author_id` for `object_id`
    /// (the policy object for keyrings) at `epoch`.
    pub fn envelope_binding(
        &self,
        object_id: [u8; 16],
        epoch: u64,
        author_id: [u8; 16],
    ) -> RecordBinding {
        RecordBinding {
            kind: RecordKind::KeyEnvelope,
            vault_id: self.vault_id,
            generation: self.generation,
            epoch,
            object_id,
            author_id,
            membership_hash: self.hash,
        }
    }
}

fn payload_of(encoded: &[u8]) -> Result<&[u8], PolicyError> {
    // The wrapper is re-parsed here only to borrow its payload; the caller
    // has already bounded and parsed it once.
    let parsed = UnverifiedRecord::parse(encoded, MAX_POLICY_BYTES)?;
    Ok(parsed.untrusted_payload())
}

fn check_device_entries(devices: &[DeviceEntry]) -> Result<(), PolicyError> {
    if devices.is_empty() || devices.len() > MAX_DEVICES {
        return Err(PolicyError::InvalidDeviceSet);
    }
    for (index, device) in devices.iter().enumerate() {
        if !crate::ed25519_point_encoding_precheck(&device.signing_key)
            || device.encryption_key.iter().all(|byte| *byte == 0)
            || device.device_id == POLICY_OBJECT_ID
        {
            return Err(PolicyError::InvalidDeviceSet);
        }
        if devices[..index].iter().any(|other| {
            other.device_id == device.device_id
                || other.signing_key == device.signing_key
                || other.encryption_key == device.encryption_key
        }) {
            return Err(PolicyError::InvalidDeviceSet);
        }
    }
    Ok(())
}

fn check_recovery_keys(payload: &PolicyPayload) -> Result<(), PolicyError> {
    if !crate::ed25519_point_encoding_precheck(&payload.recovery_signing_key)
        || payload
            .recovery_encryption_key
            .iter()
            .all(|byte| *byte == 0)
    {
        return Err(PolicyError::InvalidRecoveryKeys);
    }
    if payload.devices.iter().any(|device| {
        device.signing_key == payload.recovery_signing_key
            || device.encryption_key == payload.recovery_encryption_key
    }) {
        return Err(PolicyError::InvalidRecoveryKeys);
    }
    Ok(())
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
