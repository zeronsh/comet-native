//! Key envelopes (RFC 0001 §5): signed-wrapper kind = KeyEnvelope records
//! that carry either
//!
//! * a **keyring envelope** — the workspace keyring HPKE-sealed to one
//!   recipient (an approved device or the recovery authority), or
//! * an **object key envelope** — one object's random content root key
//!   wrapped under the epoch key that created it (AES-256-GCM with a fresh
//!   per-record derived key, the same construction as content records).
//!
//! Payload map:
//!
//! ```text
//!   0  envelope version   unsigned, exactly 1
//!   1  recipient kind     1 device, 2 recovery authority, 3 epoch key
//!   2  recipient id       16 bytes (device/authority id, or the epoch as a
//!                         big-endian u64 right-aligned in 16 bytes)
//!   3  encapsulation      32 bytes (HPKE enc, or the derivation salt)
//!   4  ciphertext + tag   bounded byte string
//! ```
//!
//! Fields 0..3 are the protected header H; C is the wrapper context
//! (`record::context_bytes`). HPKE info = "zeron/keyring-envelope/v1" || 0x00
//! || C || H. Object keys derive K = HKDF(epoch key, salt,
//! "zeron/object-key/v1" || 0x00 || C || H) and AAD = "zeron/object-key/aad/v1"
//! || 0x00 || C || H, then encrypt id || key with a zero nonce exactly once.

use crate::content::{ContentError, ContentKey, DeviceSigner, KeyScope};
use crate::hpke::{self, HpkePrivateKey, HpkePublicKey};
use crate::keyring::{Keyring, KeyringError, MAX_KEYRING_BYTES};
use crate::record::{self, Reader, RecordBinding, RecordError, RecordKind, UnverifiedRecord};
use crate::{CryptoError, hkdf_sha256, open_aes256_gcm};
use ring::aead;
use std::fmt;
use zeroize::Zeroizing;

const KEYRING_INFO_DOMAIN: &[u8] = b"zeron/keyring-envelope/v1\0";
const OBJECT_KEY_DOMAIN: &[u8] = b"zeron/object-key/v1\0";
const OBJECT_AAD_DOMAIN: &[u8] = b"zeron/object-key/aad/v1\0";
const PAYLOAD_OVERHEAD: usize = 128;
pub const MAX_ENVELOPE_PAYLOAD_BYTES: usize = MAX_KEYRING_BYTES + 16 + PAYLOAD_OVERHEAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    Record(RecordError),
    Crypto(CryptoError),
    Keyring(KeyringError),
    Content(ContentError),
    WrongKind,
    WrongRecipient,
    WrongScope,
    UnsupportedFormat,
    SizeLimitExceeded,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for EnvelopeError {}
impl From<RecordError> for EnvelopeError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
impl From<CryptoError> for EnvelopeError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}
impl From<KeyringError> for EnvelopeError {
    fn from(error: KeyringError) -> Self {
        Self::Keyring(error)
    }
}
impl From<ContentError> for EnvelopeError {
    fn from(error: ContentError) -> Self {
        Self::Content(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum RecipientKind {
    Device = 1,
    Recovery = 2,
    Epoch = 3,
}

impl TryFrom<u64> for RecipientKind {
    type Error = EnvelopeError;
    fn try_from(value: u64) -> Result<Self, EnvelopeError> {
        match value {
            1 => Ok(Self::Device),
            2 => Ok(Self::Recovery),
            3 => Ok(Self::Epoch),
            _ => Err(EnvelopeError::UnsupportedFormat),
        }
    }
}

pub struct SealedEnvelope {
    binding: RecordBinding,
    revision_id: [u8; 16],
    encoded: Vec<u8>,
}

impl SealedEnvelope {
    pub fn binding(&self) -> &RecordBinding {
        &self.binding
    }
    pub fn revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
    pub fn into_encoded(self) -> Vec<u8> {
        self.encoded
    }
}

impl fmt::Debug for SealedEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SealedEnvelope([REDACTED])")
    }
}

/// Epoch numbers occupy the recipient-id slot right-aligned.
pub fn epoch_recipient_id(epoch: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&epoch.to_be_bytes());
    id
}

/// Seal the whole keyring to `recipient` and sign the envelope.
pub fn seal_keyring(
    binding: &RecordBinding,
    recipient_kind: RecipientKind,
    recipient_id: &[u8; 16],
    recipient_key: &HpkePublicKey,
    keyring: &Keyring,
    signer: &DeviceSigner,
) -> Result<SealedEnvelope, EnvelopeError> {
    check_binding(binding, signer)?;
    if recipient_kind == RecipientKind::Epoch {
        return Err(EnvelopeError::WrongRecipient);
    }
    let plaintext = Zeroizing::new(keyring.encode());
    if plaintext.len() > MAX_KEYRING_BYTES {
        return Err(EnvelopeError::SizeLimitExceeded);
    }
    let revision_id = fresh_revision()?;
    let context = record::context_bytes(binding, &revision_id)?;
    let header = header(3, recipient_kind, recipient_id);
    let info = concat(KEYRING_INFO_DOMAIN, &context, &header);
    let sealed = hpke::seal(recipient_key, &info, &[], &plaintext, MAX_KEYRING_BYTES)?;
    let mut payload = header_with_encapsulation(recipient_kind, recipient_id, &sealed.enc);
    record::bytes_field(&mut payload, 4, &sealed.ciphertext);
    finish(binding, revision_id, payload, signer)
}

/// Verify and open a keyring envelope addressed to this recipient.
pub fn open_keyring(
    encoded: &[u8],
    expected: &RecordBinding,
    recipient_kind: RecipientKind,
    recipient_id: &[u8; 16],
    recipient_key: &HpkePrivateKey,
    trusted_public_key: &[u8],
) -> Result<Keyring, EnvelopeError> {
    if expected.kind != RecordKind::KeyEnvelope {
        return Err(EnvelopeError::WrongKind);
    }
    let record = UnverifiedRecord::parse(encoded, MAX_ENVELOPE_PAYLOAD_BYTES)?
        .verify(expected, trusted_public_key)?;
    let parsed = parse_payload(record.payload(), MAX_KEYRING_BYTES + 16)?;
    if parsed.recipient_kind != recipient_kind || parsed.recipient_id != *recipient_id {
        return Err(EnvelopeError::WrongRecipient);
    }
    if parsed.recipient_kind == RecipientKind::Epoch {
        return Err(EnvelopeError::WrongRecipient);
    }
    let context = record::context_bytes(expected, record.revision_id())?;
    let header = header(3, parsed.recipient_kind, &parsed.recipient_id);
    let info = concat(KEYRING_INFO_DOMAIN, &context, &header);
    let plaintext = hpke::open(
        recipient_key,
        &parsed.encapsulation,
        &info,
        &[],
        parsed.ciphertext,
        MAX_KEYRING_BYTES,
    )?;
    Ok(Keyring::decode(plaintext.as_bytes())?)
}

/// Wrap one object's content key under the epoch key of `binding.epoch`.
/// The binding's object ID must be the key's object and the key's scope must
/// match the binding exactly.
pub fn wrap_object_key(
    binding: &RecordBinding,
    epoch_key: &[u8; 32],
    key: &ContentKey,
    signer: &DeviceSigner,
) -> Result<SealedEnvelope, EnvelopeError> {
    check_binding(binding, signer)?;
    if key.scope() != KeyScope::from(binding) {
        return Err(EnvelopeError::WrongScope);
    }
    let recipient_id = epoch_recipient_id(binding.epoch);
    let revision_id = fresh_revision()?;
    let mut salt = [0; 32];
    crate::fill_random(&mut salt)?;
    let context = record::context_bytes(binding, &revision_id)?;
    let header = header_with_encapsulation(RecipientKind::Epoch, &recipient_id, &salt);
    let derived = hkdf_sha256(
        epoch_key,
        &salt,
        &concat(OBJECT_KEY_DOMAIN, &context, &header_prefix(&header)),
        32,
    )?;
    let aad = concat(OBJECT_AAD_DOMAIN, &context, &header_prefix(&header));
    let sealing_key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, derived.as_bytes())
            .map_err(|_| CryptoError::InvalidKeyLength)?,
    );
    let mut buffer = Zeroizing::new(Vec::with_capacity(64));
    buffer.extend_from_slice(key.identifier());
    buffer.extend_from_slice(key.expose_secret());
    sealing_key
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key([0; 12]),
            aead::Aad::from(&aad),
            &mut *buffer,
        )
        .map_err(|_| CryptoError::AuthenticationFailed)?;
    let mut payload = header;
    record::bytes_field(&mut payload, 4, &buffer);
    finish(binding, revision_id, payload, signer)
}

/// Verify and unwrap an object key envelope with the epoch key named by the
/// expected binding.
pub fn unwrap_object_key(
    encoded: &[u8],
    expected: &RecordBinding,
    epoch_key: &[u8; 32],
    trusted_public_key: &[u8],
) -> Result<ContentKey, EnvelopeError> {
    if expected.kind != RecordKind::KeyEnvelope {
        return Err(EnvelopeError::WrongKind);
    }
    let record = UnverifiedRecord::parse(encoded, MAX_ENVELOPE_PAYLOAD_BYTES)?
        .verify(expected, trusted_public_key)?;
    let parsed = parse_payload(record.payload(), 48)?;
    if parsed.recipient_kind != RecipientKind::Epoch
        || parsed.recipient_id != epoch_recipient_id(expected.epoch)
    {
        return Err(EnvelopeError::WrongRecipient);
    }
    let context = record::context_bytes(expected, record.revision_id())?;
    let header = header_with_encapsulation(
        RecipientKind::Epoch,
        &parsed.recipient_id,
        &parsed.encapsulation,
    );
    let derived = hkdf_sha256(
        epoch_key,
        &parsed.encapsulation,
        &concat(OBJECT_KEY_DOMAIN, &context, &header_prefix(&header)),
        32,
    )?;
    let aad = concat(OBJECT_AAD_DOMAIN, &context, &header_prefix(&header));
    let plaintext = open_aes256_gcm(derived.as_bytes(), &[0; 12], &aad, parsed.ciphertext, 48)?;
    let bytes = plaintext.as_bytes();
    if bytes.len() != 48 {
        return Err(EnvelopeError::UnsupportedFormat);
    }
    let identifier: [u8; 16] = bytes[..16]
        .try_into()
        .map_err(|_| EnvelopeError::UnsupportedFormat)?;
    Ok(ContentKey::from_bytes(
        KeyScope::from(expected),
        identifier,
        &bytes[16..],
    )?)
}

struct ParsedPayload<'a> {
    recipient_kind: RecipientKind,
    recipient_id: [u8; 16],
    encapsulation: [u8; 32],
    ciphertext: &'a [u8],
}

fn parse_payload(payload: &[u8], max_plaintext: usize) -> Result<ParsedPayload<'_>, EnvelopeError> {
    let mut reader = Reader::new(payload);
    if reader.argument(5)? != 5 {
        return Err(EnvelopeError::UnsupportedFormat);
    }
    if reader.uint_field(0)? != 1 {
        return Err(EnvelopeError::UnsupportedFormat);
    }
    let recipient_kind = RecipientKind::try_from(reader.uint_field(1)?)?;
    let recipient_id = reader.fixed_field(2)?;
    let encapsulation = reader.fixed_field(3)?;
    let ciphertext = reader.bytes_field(4, max_plaintext + 16)?;
    reader.finish()?;
    if ciphertext.len() < 16 {
        return Err(EnvelopeError::UnsupportedFormat);
    }
    Ok(ParsedPayload {
        recipient_kind,
        recipient_id,
        encapsulation,
        ciphertext,
    })
}

fn check_binding(binding: &RecordBinding, signer: &DeviceSigner) -> Result<(), EnvelopeError> {
    if binding.kind != RecordKind::KeyEnvelope {
        return Err(EnvelopeError::WrongKind);
    }
    if binding.author_id != *signer.author_id() {
        return Err(EnvelopeError::Content(ContentError::WrongAuthor));
    }
    Ok(())
}

fn fresh_revision() -> Result<[u8; 16], EnvelopeError> {
    let mut revision_id = [0; 16];
    crate::fill_random(&mut revision_id)?;
    Ok(revision_id)
}

fn header(count: u64, recipient_kind: RecipientKind, recipient_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    record::argument(&mut out, 5, count);
    record::uint_field(&mut out, 0, 1);
    record::uint_field(&mut out, 1, recipient_kind as u64);
    record::bytes_field(&mut out, 2, recipient_id);
    out
}

fn header_with_encapsulation(
    recipient_kind: RecipientKind,
    recipient_id: &[u8; 16],
    encapsulation: &[u8; 32],
) -> Vec<u8> {
    let mut out = header(5, recipient_kind, recipient_id);
    record::bytes_field(&mut out, 3, encapsulation);
    out
}

/// The object-key derivation binds fields 0..3 as a length-4 map, distinct
/// from the length-5 payload map that also carries the ciphertext.
fn header_prefix(header_with_encapsulation: &[u8]) -> Vec<u8> {
    let mut out = header_with_encapsulation.to_vec();
    out[0] = 0xa4;
    out
}

fn concat(domain: &[u8], context: &[u8], header: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + context.len() + header.len());
    out.extend_from_slice(domain);
    out.extend_from_slice(context);
    out.extend_from_slice(header);
    out
}

fn finish(
    binding: &RecordBinding,
    revision_id: [u8; 16],
    payload: Vec<u8>,
    signer: &DeviceSigner,
) -> Result<SealedEnvelope, EnvelopeError> {
    let input = record::signing_bytes(binding, &revision_id, &payload, MAX_ENVELOPE_PAYLOAD_BYTES)?;
    let signature = signer.sign_bytes(&input)?;
    let encoded = record::encode_signed(
        binding,
        &revision_id,
        &payload,
        &signature,
        MAX_ENVELOPE_PAYLOAD_BYTES,
    )?;
    Ok(SealedEnvelope {
        binding: *binding,
        revision_id,
        encoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(kind: RecordKind, object_id: [u8; 16], epoch: u64) -> RecordBinding {
        RecordBinding {
            kind,
            vault_id: [1; 16],
            generation: [2; 16],
            epoch,
            object_id,
            author_id: [4; 16],
            membership_hash: [5; 32],
        }
    }

    #[test]
    fn keyring_envelope_round_trips_and_binds_recipient() {
        let signer = DeviceSigner::from_seed([4; 16], &[8; 32]).unwrap();
        let recipient = HpkePrivateKey::generate().unwrap();
        let other = HpkePrivateKey::generate().unwrap();
        let mut keyring = Keyring::new();
        keyring.insert_fresh(1).unwrap();
        keyring.insert_fresh(2).unwrap();
        let context = binding(RecordKind::KeyEnvelope, [0; 16], 2);
        let sealed = seal_keyring(
            &context,
            RecipientKind::Device,
            &[7; 16],
            &recipient.public_key(),
            &keyring,
            &signer,
        )
        .unwrap();
        let opened = open_keyring(
            sealed.encoded(),
            &context,
            RecipientKind::Device,
            &[7; 16],
            &recipient,
            signer.public_key(),
        )
        .unwrap();
        assert_eq!(opened.epoch_key(1), keyring.epoch_key(1));
        assert_eq!(opened.epoch_key(2), keyring.epoch_key(2));
        assert!(matches!(
            open_keyring(
                sealed.encoded(),
                &context,
                RecipientKind::Recovery,
                &[7; 16],
                &recipient,
                signer.public_key()
            ),
            Err(EnvelopeError::WrongRecipient)
        ));
        assert!(matches!(
            open_keyring(
                sealed.encoded(),
                &context,
                RecipientKind::Device,
                &[8; 16],
                &recipient,
                signer.public_key()
            ),
            Err(EnvelopeError::WrongRecipient)
        ));
        assert!(
            open_keyring(
                sealed.encoded(),
                &context,
                RecipientKind::Device,
                &[7; 16],
                &other,
                signer.public_key()
            )
            .is_err()
        );
        let mut wrong = context;
        wrong.epoch = 3;
        assert!(matches!(
            open_keyring(
                sealed.encoded(),
                &wrong,
                RecipientKind::Device,
                &[7; 16],
                &recipient,
                signer.public_key()
            ),
            Err(EnvelopeError::Record(RecordError::ContextMismatch))
        ));
        let mut tampered = sealed.encoded().to_vec();
        let last = tampered.len() - 70;
        tampered[last] ^= 1;
        assert!(
            open_keyring(
                &tampered,
                &context,
                RecipientKind::Device,
                &[7; 16],
                &recipient,
                signer.public_key()
            )
            .is_err()
        );
        assert!(matches!(
            seal_keyring(
                &binding(RecordKind::Content, [0; 16], 2),
                RecipientKind::Device,
                &[7; 16],
                &recipient.public_key(),
                &keyring,
                &signer
            ),
            Err(EnvelopeError::WrongKind)
        ));
        assert_eq!(format!("{sealed:?}"), "SealedEnvelope([REDACTED])");
    }

    #[test]
    fn object_key_wrap_round_trips_and_binds_epoch_and_object() {
        let signer = DeviceSigner::from_seed([4; 16], &[8; 32]).unwrap();
        let epoch_key = [11; 32];
        let context = binding(RecordKind::KeyEnvelope, [9; 16], 2);
        let key = ContentKey::generate(KeyScope::from(&context)).unwrap();
        let wrapped = wrap_object_key(&context, &epoch_key, &key, &signer).unwrap();
        let unwrapped =
            unwrap_object_key(wrapped.encoded(), &context, &epoch_key, signer.public_key())
                .unwrap();
        assert_eq!(unwrapped.identifier(), key.identifier());
        assert_eq!(unwrapped.expose_secret(), key.expose_secret());
        assert_eq!(unwrapped.scope(), key.scope());
        assert!(
            unwrap_object_key(wrapped.encoded(), &context, &[12; 32], signer.public_key()).is_err()
        );
        let mut other_object = context;
        other_object.object_id = [10; 16];
        assert!(
            unwrap_object_key(
                wrapped.encoded(),
                &other_object,
                &epoch_key,
                signer.public_key()
            )
            .is_err()
        );
        assert!(matches!(
            wrap_object_key(&other_object, &epoch_key, &key, &signer),
            Err(EnvelopeError::WrongScope)
        ));
        // A keyring envelope is not an object key and vice versa.
        let recipient = HpkePrivateKey::generate().unwrap();
        assert!(matches!(
            open_keyring(
                wrapped.encoded(),
                &context,
                RecipientKind::Epoch,
                &epoch_recipient_id(2),
                &recipient,
                signer.public_key()
            ),
            Err(EnvelopeError::WrongRecipient)
        ));
        let again = wrap_object_key(&context, &epoch_key, &key, &signer).unwrap();
        assert_ne!(again.encoded(), wrapped.encoded());
    }
}
