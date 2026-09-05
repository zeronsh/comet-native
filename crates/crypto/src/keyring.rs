//! The workspace keyring (RFC 0001 §5): one random 32-byte wrapping key per
//! write epoch. Devices and the recovery authority receive the whole keyring
//! inside an HPKE envelope (`envelope.rs`); per-object content keys are
//! wrapped under the epoch key of the epoch that created them. Keys are
//! never discarded while ciphertext may still reference them.
//!
//! Encoding: deterministic CBOR map `{0: 1, 1: [[epoch, key32], ...]}` with
//! epochs strictly ascending.

use crate::CryptoError;
use crate::record::{self, Reader, RecordError};
use std::collections::BTreeMap;
use std::fmt;
use zeroize::Zeroizing;

pub const MAX_EPOCHS: usize = 1024;
pub const MAX_KEYRING_BYTES: usize = 16 + MAX_EPOCHS * 44;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyringError {
    Record(RecordError),
    Crypto(CryptoError),
    Malformed,
    UnsupportedVersion,
    DuplicateEpoch,
    InvalidEpoch,
    TooManyEpochs,
}

impl fmt::Display for KeyringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for KeyringError {}
impl From<RecordError> for KeyringError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}
impl From<CryptoError> for KeyringError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

#[derive(Default)]
pub struct Keyring {
    epochs: BTreeMap<u64, Zeroizing<[u8; 32]>>,
}

impl Keyring {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate a fresh random key for `epoch`; an existing epoch is never
    /// overwritten (a rotation always introduces a NEW epoch number).
    pub fn insert_fresh(&mut self, epoch: u64) -> Result<(), KeyringError> {
        if epoch == 0 {
            return Err(KeyringError::InvalidEpoch);
        }
        if self.epochs.contains_key(&epoch) {
            return Err(KeyringError::DuplicateEpoch);
        }
        if self.epochs.len() >= MAX_EPOCHS {
            return Err(KeyringError::TooManyEpochs);
        }
        let mut key = Zeroizing::new([0; 32]);
        crate::fill_random(key.as_mut())?;
        self.epochs.insert(epoch, key);
        Ok(())
    }

    pub fn insert(&mut self, epoch: u64, key: &[u8]) -> Result<(), KeyringError> {
        if epoch == 0 {
            return Err(KeyringError::InvalidEpoch);
        }
        let key: [u8; 32] = key.try_into().map_err(|_| KeyringError::Malformed)?;
        match self.epochs.get(&epoch) {
            Some(existing) if **existing == key => Ok(()),
            Some(_) => Err(KeyringError::DuplicateEpoch),
            None if self.epochs.len() >= MAX_EPOCHS => Err(KeyringError::TooManyEpochs),
            None => {
                self.epochs.insert(epoch, Zeroizing::new(key));
                Ok(())
            }
        }
    }

    /// Merge every epoch from `other`; conflicting bytes for one epoch fail.
    pub fn merge(&mut self, other: &Keyring) -> Result<(), KeyringError> {
        for (epoch, key) in &other.epochs {
            self.insert(*epoch, key.as_ref())?;
        }
        Ok(())
    }

    pub fn epoch_key(&self, epoch: u64) -> Option<&[u8; 32]> {
        self.epochs.get(&epoch).map(|key| &**key)
    }

    pub fn epochs(&self) -> impl Iterator<Item = u64> + '_ {
        self.epochs.keys().copied()
    }

    pub fn latest_epoch(&self) -> Option<u64> {
        self.epochs.keys().next_back().copied()
    }

    pub fn len(&self) -> usize {
        self.epochs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.epochs.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.epochs.len() * 44);
        record::argument(&mut out, 5, 2);
        record::uint_field(&mut out, 0, 1);
        record::argument(&mut out, 0, 1);
        record::argument(&mut out, 4, self.epochs.len() as u64);
        for (epoch, key) in &self.epochs {
            record::argument(&mut out, 4, 2);
            record::argument(&mut out, 0, *epoch);
            record::argument(&mut out, 2, 32);
            out.extend_from_slice(key.as_ref());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, KeyringError> {
        if bytes.len() > MAX_KEYRING_BYTES {
            return Err(KeyringError::TooManyEpochs);
        }
        let mut reader = Reader::new(bytes);
        if reader.argument(5)? != 2 {
            return Err(KeyringError::Malformed);
        }
        if reader.uint_field(0)? != 1 {
            return Err(KeyringError::UnsupportedVersion);
        }
        if reader.argument(0)? != 1 {
            return Err(KeyringError::Malformed);
        }
        let count = reader.argument(4)?;
        if count > MAX_EPOCHS as u64 {
            return Err(KeyringError::TooManyEpochs);
        }
        let mut keyring = Self::new();
        let mut previous = 0;
        for _ in 0..count {
            if reader.argument(4)? != 2 {
                return Err(KeyringError::Malformed);
            }
            let epoch = reader.argument(0)?;
            if epoch <= previous {
                return Err(KeyringError::Malformed);
            }
            previous = epoch;
            let key = reader.fixed_bytes::<32>()?;
            keyring.epochs.insert(epoch, Zeroizing::new(key));
        }
        reader.finish()?;
        Ok(keyring)
    }
}

impl fmt::Debug for Keyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Keyring({} epochs, [REDACTED])",
            self.epochs.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_malformed() {
        let mut keyring = Keyring::new();
        keyring.insert_fresh(1).unwrap();
        keyring.insert_fresh(3).unwrap();
        assert!(matches!(
            keyring.insert_fresh(1),
            Err(KeyringError::DuplicateEpoch)
        ));
        assert!(matches!(
            keyring.insert_fresh(0),
            Err(KeyringError::InvalidEpoch)
        ));
        let encoded = keyring.encode();
        let decoded = Keyring::decode(&encoded).unwrap();
        assert_eq!(decoded.epoch_key(1), keyring.epoch_key(1));
        assert_eq!(decoded.epoch_key(3), keyring.epoch_key(3));
        assert_eq!(decoded.latest_epoch(), Some(3));
        assert_eq!(decoded.epochs().collect::<Vec<_>>(), vec![1, 3]);
        assert!(Keyring::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(Keyring::decode(&trailing).is_err());
        // Descending epochs are non-canonical.
        let mut swapped = Keyring::new();
        swapped.insert(3, keyring.epoch_key(3).unwrap()).unwrap();
        swapped.insert(1, keyring.epoch_key(1).unwrap()).unwrap();
        assert_eq!(swapped.encode(), encoded, "BTreeMap orders on encode");
        let mut bytes = encoded.clone();
        // Patch epoch 1 -> 4 (single byte, < 24) makes the order 4, 3.
        let position = bytes.iter().position(|byte| *byte == 0x01).unwrap();
        let _ = position;
        bytes[8] = 0x04;
        assert!(Keyring::decode(&bytes).is_err());
        assert_eq!(format!("{keyring:?}"), "Keyring(2 epochs, [REDACTED])");
    }

    #[test]
    fn merge_keeps_history_and_detects_conflicts() {
        let mut first = Keyring::new();
        first.insert_fresh(1).unwrap();
        let mut second = Keyring::new();
        second.insert_fresh(2).unwrap();
        second.merge(&first).unwrap();
        assert_eq!(second.len(), 2);
        let mut conflicting = Keyring::new();
        conflicting.insert_fresh(1).unwrap();
        assert!(matches!(
            second.merge(&conflicting),
            Err(KeyringError::DuplicateEpoch)
        ));
    }
}
