//! Recovery kit (RFC 0001 §4.1, §6.3; plan Q4-A): a random 256-bit secret
//! shown once as grouped, checksummed Base32, from which separate signing and
//! encryption keys are derived with labeled HKDF. Ordinary devices never
//! retain the secret; they hold only the derived PUBLIC keys (in the
//! membership policy) so they can re-encrypt recovery envelopes after
//! rotation.

use crate::content::DeviceSigner;
use crate::hpke::HpkePrivateKey;
use crate::{CryptoError, hkdf_sha256, sha256};
use std::fmt;
use zeroize::Zeroizing;

const KIT_DOMAIN: &[u8] = b"zeron/recovery-kit/v1\0";
const SIGNING_LABEL: &[u8] = b"zeron/recovery/sign/v1";
const ENCRYPTION_LABEL: &[u8] = b"zeron/recovery/hpke/v1";
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const GROUP: usize = 5;
/// 32 secret bytes + 2 checksum bytes = 272 bits = 55 Base32 symbols.
const KIT_SYMBOLS: usize = 55;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryError {
    Crypto(CryptoError),
    InvalidCharacter,
    InvalidLength,
    ChecksumMismatch,
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for RecoveryError {}
impl From<CryptoError> for RecoveryError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

pub struct RecoverySecret(Zeroizing<[u8; 32]>);

impl RecoverySecret {
    pub fn generate() -> Result<Self, RecoveryError> {
        let mut bytes = Zeroizing::new([0; 32]);
        crate::fill_random(bytes.as_mut())?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RecoveryError> {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| RecoveryError::InvalidLength)?;
        Ok(Self(Zeroizing::new(bytes)))
    }

    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }

    /// The user-facing kit text: 11 dash-separated groups of 5 symbols.
    pub fn to_kit(&self) -> Zeroizing<String> {
        let mut payload = [0u8; 34];
        payload[..32].copy_from_slice(self.0.as_ref());
        payload[32..].copy_from_slice(&self.checksum());
        let symbols = base32_encode(&payload);
        let mut text = String::with_capacity(KIT_SYMBOLS + KIT_SYMBOLS / GROUP);
        for (index, symbol) in symbols.iter().enumerate() {
            if index > 0 && index % GROUP == 0 {
                text.push('-');
            }
            text.push(*symbol as char);
        }
        Zeroizing::new(text)
    }

    /// Parse kit text. Whitespace and dashes are ignored, case is not
    /// significant, and the checksum catches transcription errors.
    pub fn from_kit(text: &str) -> Result<Self, RecoveryError> {
        let mut symbols = Zeroizing::new(Vec::with_capacity(KIT_SYMBOLS));
        for character in text.chars() {
            if character.is_whitespace() || character == '-' {
                continue;
            }
            let upper = character.to_ascii_uppercase();
            if !upper.is_ascii() {
                return Err(RecoveryError::InvalidCharacter);
            }
            symbols.push(upper as u8);
        }
        if symbols.len() != KIT_SYMBOLS {
            return Err(RecoveryError::InvalidLength);
        }
        let payload = base32_decode(&symbols)?;
        let secret = Self::from_bytes(&payload[..32])?;
        if payload[32..34] != secret.checksum() {
            return Err(RecoveryError::ChecksumMismatch);
        }
        Ok(secret)
    }

    fn checksum(&self) -> [u8; 2] {
        let digest = sha256(&[KIT_DOMAIN, self.0.as_ref()]);
        [digest[0], digest[1]]
    }

    fn signing_seed(&self) -> Result<Zeroizing<[u8; 32]>, RecoveryError> {
        let derived = hkdf_sha256(self.0.as_ref(), &[], SIGNING_LABEL, 32)?;
        let mut seed = Zeroizing::new([0; 32]);
        seed.copy_from_slice(derived.as_bytes());
        Ok(seed)
    }

    /// The recovery signing identity; its author ID is derived from its
    /// public key (`policy::recovery_authority_id`).
    pub fn signer(&self) -> Result<DeviceSigner, RecoveryError> {
        let seed = self.signing_seed()?;
        let probe = DeviceSigner::from_seed([0; 16], seed.as_ref())
            .map_err(|_| RecoveryError::Crypto(CryptoError::InvalidKeyLength))?;
        let public: [u8; 32] = probe
            .public_key()
            .try_into()
            .map_err(|_| RecoveryError::Crypto(CryptoError::InvalidKeyLength))?;
        DeviceSigner::from_seed(crate::policy::recovery_authority_id(&public), seed.as_ref())
            .map_err(|_| RecoveryError::Crypto(CryptoError::InvalidKeyLength))
    }

    pub fn signing_public_key(&self) -> Result<[u8; 32], RecoveryError> {
        self.signer()?
            .public_key()
            .try_into()
            .map_err(|_| RecoveryError::Crypto(CryptoError::InvalidKeyLength))
    }

    pub fn encryption_key(&self) -> Result<HpkePrivateKey, RecoveryError> {
        let derived = hkdf_sha256(self.0.as_ref(), &[], ENCRYPTION_LABEL, 32)?;
        Ok(HpkePrivateKey::from_bytes(derived.as_bytes())?)
    }
}

impl fmt::Debug for RecoverySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecoverySecret([REDACTED])")
    }
}

fn base32_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize]);
    }
    out
}

fn base32_decode(symbols: &[u8]) -> Result<Zeroizing<Vec<u8>>, RecoveryError> {
    let mut out = Zeroizing::new(Vec::with_capacity(symbols.len() * 5 / 8));
    let mut buffer = 0u32;
    let mut bits = 0;
    for symbol in symbols {
        let value = ALPHABET
            .iter()
            .position(|candidate| candidate == symbol)
            .ok_or(RecoveryError::InvalidCharacter)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    // Leftover bits are padding and must be zero for a canonical kit.
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return Err(RecoveryError::InvalidCharacter);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_round_trips_with_grouping_and_tolerant_parsing() {
        let secret = RecoverySecret::generate().unwrap();
        let kit = secret.to_kit();
        assert_eq!(kit.len(), KIT_SYMBOLS + 10);
        assert_eq!(kit.split('-').count(), 11);
        assert!(kit.split('-').all(|group| group.len() == 5));
        let parsed = RecoverySecret::from_kit(&kit).unwrap();
        assert_eq!(parsed.expose_secret(), secret.expose_secret());
        let sloppy = kit.to_lowercase().replace('-', " ");
        assert_eq!(
            RecoverySecret::from_kit(&sloppy).unwrap().expose_secret(),
            secret.expose_secret()
        );
        assert_eq!(format!("{secret:?}"), "RecoverySecret([REDACTED])");
    }

    #[test]
    fn kit_detects_transcription_errors() {
        let secret = RecoverySecret::from_bytes(&[7; 32]).unwrap();
        let kit = secret.to_kit();
        let mut damaged: Vec<char> = kit.chars().collect();
        damaged[0] = if damaged[0] == 'A' { 'B' } else { 'A' };
        let damaged: String = damaged.into_iter().collect();
        assert!(matches!(
            RecoverySecret::from_kit(&damaged),
            Err(RecoveryError::ChecksumMismatch)
        ));
        assert!(matches!(
            RecoverySecret::from_kit(&kit[..kit.len() - 1]),
            Err(RecoveryError::InvalidLength)
        ));
        assert!(matches!(
            RecoverySecret::from_kit(&kit.replace('A', "1")),
            Err(RecoveryError::InvalidCharacter)
        ));
        // Fixed vector pins the alphabet/checksum across languages.
        assert_eq!(
            &*kit,
            "A4DQO-BYHA4-DQOBY-HA4DQ-OBYHA-4DQOB-YHA4D-QOBYH-A4DQO-BYHA4-D2MCI"
        );
    }

    #[test]
    fn derived_keys_are_stable_and_separate() {
        let secret = RecoverySecret::from_bytes(&[9; 32]).unwrap();
        let again = RecoverySecret::from_bytes(&[9; 32]).unwrap();
        assert_eq!(secret.signing_public_key(), again.signing_public_key());
        assert_eq!(
            secret.encryption_key().unwrap().public_key(),
            again.encryption_key().unwrap().public_key()
        );
        assert_ne!(
            secret.signing_public_key().unwrap().to_vec(),
            secret
                .encryption_key()
                .unwrap()
                .public_key()
                .as_bytes()
                .to_vec()
        );
        let signer = secret.signer().unwrap();
        assert_eq!(
            *signer.author_id(),
            crate::policy::recovery_authority_id(&secret.signing_public_key().unwrap())
        );
        let other = RecoverySecret::from_bytes(&[10; 32]).unwrap();
        assert_ne!(secret.signing_public_key(), other.signing_public_key());
    }
}
