use crate::verify_ed25519;
use std::fmt;

const DOMAIN: &[u8] = b"zeron/signed-record/v1\0";
const MAX_OVERHEAD: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    Malformed,
    NonCanonical,
    UnsupportedVersion,
    UnsupportedKind,
    InvalidEpoch,
    SizeLimitExceeded,
    ContextMismatch,
    InvalidSignature,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for RecordError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum RecordKind {
    Policy = 1,
    KeyEnvelope = 2,
    Content = 3,
}

impl TryFrom<u64> for RecordKind {
    type Error = RecordError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Policy),
            2 => Ok(Self::KeyEnvelope),
            3 => Ok(Self::Content),
            _ => Err(RecordError::UnsupportedKind),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordBinding {
    pub kind: RecordKind,
    pub vault_id: [u8; 16],
    pub generation: [u8; 16],
    pub epoch: u64,
    pub object_id: [u8; 16],
    pub author_id: [u8; 16],
    pub membership_hash: [u8; 32],
}

pub struct UnverifiedRecord<'a> {
    binding: RecordBinding,
    revision_id: [u8; 16],
    payload: &'a [u8],
    signature: [u8; 64],
    max_payload_bytes: usize,
}

impl<'a> UnverifiedRecord<'a> {
    pub fn parse(encoded: &'a [u8], max_payload_bytes: usize) -> Result<Self, RecordError> {
        if encoded.len() > total_limit(max_payload_bytes)? {
            return Err(RecordError::SizeLimitExceeded);
        }
        let mut reader = Reader(encoded);
        if reader.argument(5)? != 11 {
            return Err(RecordError::Malformed);
        }
        if reader.uint_field(0)? != 1 {
            return Err(RecordError::UnsupportedVersion);
        }
        let kind = RecordKind::try_from(reader.uint_field(1)?)?;
        let vault_id = reader.fixed_field(2)?;
        let generation = reader.fixed_field(3)?;
        let epoch = reader.uint_field(4)?;
        if epoch == 0 {
            return Err(RecordError::InvalidEpoch);
        }
        let object_id = reader.fixed_field(5)?;
        let author_id = reader.fixed_field(6)?;
        let revision_id = reader.fixed_field(7)?;
        let membership_hash = reader.fixed_field(8)?;
        let payload = reader.bytes_field(9, max_payload_bytes)?;
        let signature = reader.fixed_field(10)?;
        if !reader.0.is_empty() {
            return Err(RecordError::Malformed);
        }
        Ok(Self {
            binding: RecordBinding {
                kind,
                vault_id,
                generation,
                epoch,
                object_id,
                author_id,
                membership_hash,
            },
            revision_id,
            payload,
            signature,
            max_payload_bytes,
        })
    }

    pub fn untrusted_binding(&self) -> &RecordBinding {
        &self.binding
    }

    pub fn untrusted_revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }

    pub fn verify(
        self,
        expected: &RecordBinding,
        trusted_public_key: &[u8],
    ) -> Result<VerifiedRecord<'a>, RecordError> {
        if &self.binding != expected {
            return Err(RecordError::ContextMismatch);
        }
        let input = signing_bytes(
            &self.binding,
            &self.revision_id,
            self.payload,
            self.max_payload_bytes,
        )?;
        verify_ed25519(trusted_public_key, &input, &self.signature)
            .map_err(|_| RecordError::InvalidSignature)?;
        Ok(VerifiedRecord {
            binding: self.binding,
            revision_id: self.revision_id,
            payload: self.payload,
        })
    }
}

impl fmt::Debug for UnverifiedRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnverifiedRecord([REDACTED])")
    }
}

pub struct VerifiedRecord<'a> {
    binding: RecordBinding,
    revision_id: [u8; 16],
    payload: &'a [u8],
}

impl VerifiedRecord<'_> {
    pub fn binding(&self) -> &RecordBinding {
        &self.binding
    }

    pub fn revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }

    pub fn payload(&self) -> &[u8] {
        self.payload
    }
}

impl fmt::Debug for VerifiedRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifiedRecord([REDACTED])")
    }
}

pub fn signing_bytes(
    binding: &RecordBinding,
    revision_id: &[u8; 16],
    payload: &[u8],
    max_payload_bytes: usize,
) -> Result<Vec<u8>, RecordError> {
    let mut out = buffer(binding, payload, max_payload_bytes)?;
    out.extend_from_slice(DOMAIN);
    fields(&mut out, 10, binding, revision_id, payload);
    Ok(out)
}

pub fn encode_signed(
    binding: &RecordBinding,
    revision_id: &[u8; 16],
    payload: &[u8],
    signature: &[u8; 64],
    max_payload_bytes: usize,
) -> Result<Vec<u8>, RecordError> {
    let mut out = buffer(binding, payload, max_payload_bytes)?;
    fields(&mut out, 11, binding, revision_id, payload);
    bytes_field(&mut out, 10, signature);
    Ok(out)
}

fn total_limit(payload_limit: usize) -> Result<usize, RecordError> {
    payload_limit
        .checked_add(MAX_OVERHEAD)
        .ok_or(RecordError::SizeLimitExceeded)
}

fn buffer(
    binding: &RecordBinding,
    payload: &[u8],
    max_payload_bytes: usize,
) -> Result<Vec<u8>, RecordError> {
    total_limit(max_payload_bytes)?;
    if binding.epoch == 0 {
        return Err(RecordError::InvalidEpoch);
    }
    if payload.len() > max_payload_bytes {
        return Err(RecordError::SizeLimitExceeded);
    }
    Ok(Vec::with_capacity(payload.len() + MAX_OVERHEAD))
}

pub(crate) fn context_bytes(
    binding: &RecordBinding,
    revision_id: &[u8; 16],
) -> Result<Vec<u8>, RecordError> {
    let mut output = buffer(binding, &[], 0)?;
    header_fields(&mut output, 9, binding, revision_id);
    Ok(output)
}

fn fields(
    out: &mut Vec<u8>,
    count: u64,
    binding: &RecordBinding,
    revision_id: &[u8; 16],
    payload: &[u8],
) {
    header_fields(out, count, binding, revision_id);
    bytes_field(out, 9, payload);
}

fn header_fields(out: &mut Vec<u8>, count: u64, binding: &RecordBinding, revision_id: &[u8; 16]) {
    argument(out, 5, count);
    uint_field(out, 0, 1);
    uint_field(out, 1, binding.kind as u64);
    bytes_field(out, 2, &binding.vault_id);
    bytes_field(out, 3, &binding.generation);
    uint_field(out, 4, binding.epoch);
    bytes_field(out, 5, &binding.object_id);
    bytes_field(out, 6, &binding.author_id);
    bytes_field(out, 7, revision_id);
    bytes_field(out, 8, &binding.membership_hash);
}

pub(crate) fn uint_field(out: &mut Vec<u8>, key: u64, value: u64) {
    argument(out, 0, key);
    argument(out, 0, value);
}

pub(crate) fn bytes_field(out: &mut Vec<u8>, key: u64, value: &[u8]) {
    argument(out, 0, key);
    argument(out, 2, value.len() as u64);
    out.extend_from_slice(value);
}

pub(crate) fn argument(out: &mut Vec<u8>, major: u8, value: u64) {
    if value < 24 {
        out.push((major << 5) | value as u8);
        return;
    }
    let (width, additional) = match value {
        24..=0xff => (1, 24),
        0x100..=0xffff => (2, 25),
        0x10000..=0xffffffff => (4, 26),
        _ => (8, 27),
    };
    out.push((major << 5) | additional);
    out.extend_from_slice(&value.to_be_bytes()[8 - width..]);
}

pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub(crate) fn finish(&self) -> Result<(), RecordError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(RecordError::Malformed)
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], RecordError> {
        if length > self.0.len() {
            return Err(RecordError::Malformed);
        }
        let (value, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(value)
    }

    pub(crate) fn argument(&mut self, major: u8) -> Result<u64, RecordError> {
        let head = self.take(1)?[0];
        if head >> 5 != major {
            return Err(RecordError::Malformed);
        }
        let (length, minimum) = match head & 31 {
            value @ 0..=23 => return Ok(value as u64),
            24 => (1, 24),
            25 => (2, 0x100),
            26 => (4, 0x10000),
            27 => (8, 0x100000000),
            _ => return Err(RecordError::Malformed),
        };
        let value = self
            .take(length)?
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
        if value < minimum {
            return Err(RecordError::NonCanonical);
        }
        Ok(value)
    }

    fn key(&mut self, expected: u64) -> Result<(), RecordError> {
        if self.argument(0)? != expected {
            return Err(RecordError::Malformed);
        }
        Ok(())
    }

    pub(crate) fn uint_field(&mut self, key: u64) -> Result<u64, RecordError> {
        self.key(key)?;
        self.argument(0)
    }

    pub(crate) fn bytes_field(&mut self, key: u64, limit: usize) -> Result<&'a [u8], RecordError> {
        self.key(key)?;
        let length =
            usize::try_from(self.argument(2)?).map_err(|_| RecordError::SizeLimitExceeded)?;
        if length > limit {
            return Err(RecordError::SizeLimitExceeded);
        }
        self.take(length)
    }

    pub(crate) fn fixed_field<const N: usize>(&mut self, key: u64) -> Result<[u8; N], RecordError> {
        self.bytes_field(key, N)?
            .try_into()
            .map_err(|_| RecordError::Malformed)
    }
}
