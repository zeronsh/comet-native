use crate::record::{self, Reader, RecordBinding, RecordError, RecordKind, UnverifiedRecord};
use crate::{CryptoError, SecretBytes, hkdf_sha256, open_aes256_gcm};
use ring::{aead, rand::SecureRandom, signature::KeyPair};
use std::fmt;
use zeroize::Zeroizing;

pub const MAX_PLAINTEXT_BYTES: usize = 16 * 1024 * 1024 - 400;
const PAYLOAD_OVERHEAD: usize = 144;
const KEY_DOMAIN: &[u8] = b"zeron/content/key/v1\0";
const AAD_DOMAIN: &[u8] = b"zeron/content/aad/v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentError {
    Record(RecordError),
    Crypto(CryptoError),
    InvalidKey,
    InvalidSigningKey,
    WrongScope,
    WrongAuthor,
    WrongKind,
    WrongKey,
    WrongPurpose,
    UnsupportedFormat,
    UnsupportedSuite,
    UnsupportedPurpose,
    SizeLimitExceeded,
    EntropyUnavailable,
}

impl fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ContentError {}
impl From<RecordError> for ContentError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
impl From<CryptoError> for ContentError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum ContentPurpose {
    ChatUpdate = 1,
    Checkpoint = 2,
    Frontier = 3,
    RegistryField = 4,
    Tail = 5,
    Diff = 6,
    Blob = 7,
    DeviceSidecar = 8,
}

impl TryFrom<u64> for ContentPurpose {
    type Error = ContentError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::ChatUpdate),
            2 => Ok(Self::Checkpoint),
            3 => Ok(Self::Frontier),
            4 => Ok(Self::RegistryField),
            5 => Ok(Self::Tail),
            6 => Ok(Self::Diff),
            7 => Ok(Self::Blob),
            8 => Ok(Self::DeviceSidecar),
            _ => Err(ContentError::UnsupportedPurpose),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyScope {
    pub vault_id: [u8; 16],
    pub generation: [u8; 16],
    pub epoch: u64,
    pub object_id: [u8; 16],
}

impl From<&RecordBinding> for KeyScope {
    fn from(binding: &RecordBinding) -> Self {
        Self {
            vault_id: binding.vault_id,
            generation: binding.generation,
            epoch: binding.epoch,
            object_id: binding.object_id,
        }
    }
}

pub struct ContentKey {
    scope: KeyScope,
    identifier: [u8; 16],
    bytes: Zeroizing<[u8; 32]>,
}

impl ContentKey {
    pub fn from_bytes(
        scope: KeyScope,
        identifier: [u8; 16],
        bytes: &[u8],
    ) -> Result<Self, ContentError> {
        if scope.epoch == 0 {
            return Err(ContentError::WrongScope);
        }
        let bytes = bytes.try_into().map_err(|_| ContentError::InvalidKey)?;
        Ok(Self {
            scope,
            identifier,
            bytes: Zeroizing::new(bytes),
        })
    }

    pub fn generate(scope: KeyScope) -> Result<Self, ContentError> {
        if scope.epoch == 0 {
            return Err(ContentError::WrongScope);
        }
        let mut material = Zeroizing::new([0; 48]);
        ring::rand::SystemRandom::new()
            .fill(material.as_mut())
            .map_err(|_| ContentError::EntropyUnavailable)?;
        let identifier = material[..16]
            .try_into()
            .map_err(|_| ContentError::InvalidKey)?;
        Self::from_bytes(scope, identifier, &material[16..])
    }

    pub fn scope(&self) -> KeyScope {
        self.scope
    }
    pub fn identifier(&self) -> &[u8; 16] {
        &self.identifier
    }
    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl fmt::Debug for ContentKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ContentKey([REDACTED])")
    }
}

pub struct DeviceSigner {
    author_id: [u8; 16],
    key_pair: ring::signature::Ed25519KeyPair,
}

impl DeviceSigner {
    pub fn from_seed(author_id: [u8; 16], seed: &[u8]) -> Result<Self, ContentError> {
        if seed.len() != 32 {
            return Err(ContentError::InvalidSigningKey);
        }
        let key_pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| ContentError::InvalidSigningKey)?;
        if !crate::ed25519_point_encoding_precheck(key_pair.public_key().as_ref()) {
            return Err(ContentError::InvalidSigningKey);
        }
        Ok(Self {
            author_id,
            key_pair,
        })
    }

    pub fn author_id(&self) -> &[u8; 16] {
        &self.author_id
    }
    pub fn public_key(&self) -> &[u8] {
        self.key_pair.public_key().as_ref()
    }
}

impl fmt::Debug for DeviceSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceSigner([REDACTED])")
    }
}

pub struct SealedContent {
    binding: RecordBinding,
    revision_id: [u8; 16],
    encoded: Vec<u8>,
}

impl SealedContent {
    pub fn binding(&self) -> &RecordBinding {
        &self.binding
    }
    pub fn revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
}

impl fmt::Debug for SealedContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SealedContent([REDACTED])")
    }
}

pub struct OpenedContent {
    revision_id: [u8; 16],
    plaintext: SecretBytes,
}

impl OpenedContent {
    pub fn revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }
    pub fn plaintext(&self) -> &SecretBytes {
        &self.plaintext
    }
}

impl fmt::Debug for OpenedContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenedContent([REDACTED])")
    }
}

pub fn seal(
    binding: &RecordBinding,
    purpose: ContentPurpose,
    key: &ContentKey,
    signer: &DeviceSigner,
    plaintext: &[u8],
    max_plaintext_bytes: usize,
) -> Result<SealedContent, ContentError> {
    seal_with_random(
        binding,
        purpose,
        key,
        signer,
        plaintext,
        max_plaintext_bytes,
        |material| {
            ring::rand::SystemRandom::new()
                .fill(material)
                .map_err(|_| ContentError::EntropyUnavailable)
        },
    )
}

fn seal_with_random(
    binding: &RecordBinding,
    purpose: ContentPurpose,
    key: &ContentKey,
    signer: &DeviceSigner,
    plaintext: &[u8],
    max_plaintext_bytes: usize,
    random: impl FnOnce(&mut [u8; 48]) -> Result<(), ContentError>,
) -> Result<SealedContent, ContentError> {
    let payload_limit = payload_limit(max_plaintext_bytes)?;
    check_scope(binding, key)?;
    if binding.author_id != signer.author_id {
        return Err(ContentError::WrongAuthor);
    }
    if plaintext.len() > max_plaintext_bytes {
        return Err(ContentError::SizeLimitExceeded);
    }
    let mut material = [0; 48];
    random(&mut material)?;
    let revision_id: [u8; 16] = material[..16]
        .try_into()
        .map_err(|_| ContentError::EntropyUnavailable)?;
    let salt: [u8; 32] = material[16..]
        .try_into()
        .map_err(|_| ContentError::EntropyUnavailable)?;
    let header = protected_header(5, purpose, &key.identifier, &salt);
    let context = record::context_bytes(binding, &revision_id)?;
    let derived = derive_key(key, &salt, &context, &header)?;
    let aad = contextual_bytes(AAD_DOMAIN, &context, &header);
    let encryption_key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, derived.as_bytes())
            .map_err(|_| ContentError::InvalidKey)?,
    );
    let mut ciphertext = Zeroizing::new(Vec::with_capacity(plaintext.len() + 16));
    ciphertext.extend_from_slice(plaintext);
    encryption_key
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key([0; 12]),
            aead::Aad::from(&aad),
            &mut *ciphertext,
        )
        .map_err(|_| ContentError::Crypto(CryptoError::AuthenticationFailed))?;
    let mut payload = protected_header(6, purpose, &key.identifier, &salt);
    record::bytes_field(&mut payload, 5, &ciphertext);
    let signature_input = record::signing_bytes(binding, &revision_id, &payload, payload_limit)?;
    let signature: [u8; 64] = signer
        .key_pair
        .sign(&signature_input)
        .as_ref()
        .try_into()
        .map_err(|_| ContentError::InvalidSigningKey)?;
    let encoded =
        record::encode_signed(binding, &revision_id, &payload, &signature, payload_limit)?;
    Ok(SealedContent {
        binding: *binding,
        revision_id,
        encoded,
    })
}

pub fn open(
    encoded: &[u8],
    expected: &RecordBinding,
    purpose: ContentPurpose,
    key: &ContentKey,
    trusted_public_key: &[u8],
    max_plaintext_bytes: usize,
) -> Result<OpenedContent, ContentError> {
    let payload_limit = payload_limit(max_plaintext_bytes)?;
    check_scope(expected, key)?;
    let record =
        UnverifiedRecord::parse(encoded, payload_limit)?.verify(expected, trusted_public_key)?;
    let mut reader = Reader::new(record.payload());
    if reader.argument(5)? != 6 {
        return Err(ContentError::UnsupportedFormat);
    }
    if reader.uint_field(0)? != 1 {
        return Err(ContentError::UnsupportedFormat);
    }
    if reader.uint_field(1)? != 1 {
        return Err(ContentError::UnsupportedSuite);
    }
    let stored_purpose = ContentPurpose::try_from(reader.uint_field(2)?)?;
    if stored_purpose != purpose {
        return Err(ContentError::WrongPurpose);
    }
    let identifier: [u8; 16] = reader.fixed_field(3)?;
    if identifier != key.identifier {
        return Err(ContentError::WrongKey);
    }
    let salt: [u8; 32] = reader.fixed_field(4)?;
    let ciphertext = reader.bytes_field(5, max_plaintext_bytes + 16)?;
    reader.finish()?;
    if ciphertext.len() < 16 {
        return Err(ContentError::UnsupportedFormat);
    }
    let header = protected_header(5, purpose, &identifier, &salt);
    let context = record::context_bytes(expected, record.revision_id())?;
    let derived = derive_key(key, &salt, &context, &header)?;
    let aad = contextual_bytes(AAD_DOMAIN, &context, &header);
    let plaintext = open_aes256_gcm(
        derived.as_bytes(),
        &[0; 12],
        &aad,
        ciphertext,
        max_plaintext_bytes,
    )?;
    Ok(OpenedContent {
        revision_id: *record.revision_id(),
        plaintext,
    })
}

fn check_scope(binding: &RecordBinding, key: &ContentKey) -> Result<(), ContentError> {
    if binding.kind != RecordKind::Content {
        return Err(ContentError::WrongKind);
    }
    if key.scope != KeyScope::from(binding) {
        return Err(ContentError::WrongScope);
    }
    Ok(())
}

fn payload_limit(max_plaintext_bytes: usize) -> Result<usize, ContentError> {
    if max_plaintext_bytes > MAX_PLAINTEXT_BYTES {
        return Err(ContentError::SizeLimitExceeded);
    }
    Ok(max_plaintext_bytes + PAYLOAD_OVERHEAD)
}

fn protected_header(
    count: u64,
    purpose: ContentPurpose,
    identifier: &[u8; 16],
    salt: &[u8; 32],
) -> Vec<u8> {
    let mut header = Vec::with_capacity(128);
    record::argument(&mut header, 5, count);
    record::uint_field(&mut header, 0, 1);
    record::uint_field(&mut header, 1, 1);
    record::uint_field(&mut header, 2, purpose as u64);
    record::bytes_field(&mut header, 3, identifier);
    record::bytes_field(&mut header, 4, salt);
    header
}

fn contextual_bytes(domain: &[u8], context: &[u8], header: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(domain.len() + context.len() + header.len());
    bytes.extend_from_slice(domain);
    bytes.extend_from_slice(context);
    bytes.extend_from_slice(header);
    bytes
}

fn derive_key(
    key: &ContentKey,
    salt: &[u8; 32],
    context: &[u8],
    header: &[u8],
) -> Result<SecretBytes, ContentError> {
    Ok(hkdf_sha256(
        key.expose_secret(),
        salt,
        &contextual_bytes(KEY_DOMAIN, context, header),
        32,
    )?)
}

#[cfg(test)]
#[path = "content_tests.rs"]
mod tests;
