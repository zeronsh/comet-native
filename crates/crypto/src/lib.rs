#![forbid(unsafe_code)]

pub mod record;

use ring::{aead, hkdf, signature};
use std::fmt;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoError {
    InvalidKeyLength,
    InvalidNonceLength,
    InvalidCiphertextLength,
    InvalidSignatureLength,
    InvalidOutputLength,
    SizeLimitExceeded,
    AuthenticationFailed,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for CryptoError {}

pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

pub fn open_aes256_gcm(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    ciphertext_and_tag: &[u8],
    max_plaintext_bytes: usize,
) -> Result<SecretBytes, CryptoError> {
    if key.len() != 32 {
        return Err(CryptoError::InvalidKeyLength);
    }
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce)
        .map_err(|_| CryptoError::InvalidNonceLength)?;
    let plaintext_len = ciphertext_and_tag
        .len()
        .checked_sub(aead::AES_256_GCM.tag_len())
        .ok_or(CryptoError::InvalidCiphertextLength)?;
    if plaintext_len > max_plaintext_bytes {
        return Err(CryptoError::SizeLimitExceeded);
    }
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| CryptoError::InvalidKeyLength)?,
    );
    let mut buffer = Zeroizing::new(ciphertext_and_tag.to_vec());
    key.open_in_place(nonce, aead::Aad::from(aad), &mut buffer)
        .map_err(|_| CryptoError::AuthenticationFailed)?;
    buffer.truncate(plaintext_len);
    Ok(SecretBytes(buffer))
}

pub fn verify_ed25519(
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), CryptoError> {
    if public_key.len() != 32 {
        return Err(CryptoError::InvalidKeyLength);
    }
    if signature.len() != 64 {
        return Err(CryptoError::InvalidSignatureLength);
    }
    if !ed25519_point_encoding_precheck(public_key)
        || !ed25519_point_encoding_precheck(&signature[..32])
        || !ed25519_scalar_encoding_precheck(&signature[32..])
    {
        return Err(CryptoError::AuthenticationFailed);
    }
    signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(message, signature)
        .map_err(|_| CryptoError::AuthenticationFailed)
}

const ED25519_FIELD_MODULUS: [u8; 32] = {
    let mut bytes = [0xff; 32];
    bytes[0] = 0xed;
    bytes[31] = 0x7f;
    bytes
};

const ED25519_SMALL_ORDER_Y: [[u8; 32]; 5] = [
    [0; 32],
    {
        let mut bytes = [0; 32];
        bytes[0] = 1;
        bytes
    },
    {
        let mut bytes = ED25519_FIELD_MODULUS;
        bytes[0] = 0xec;
        bytes
    },
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x05,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0x7a,
    ],
];

const ED25519_SCALAR_ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

pub(crate) fn ed25519_point_encoding_precheck(encoded: &[u8]) -> bool {
    let Ok(mut y) = <[u8; 32]>::try_from(encoded) else {
        return false;
    };
    y[31] &= 0x7f;
    y.iter().rev().lt(ED25519_FIELD_MODULUS.iter().rev()) && !ED25519_SMALL_ORDER_Y.contains(&y)
}

pub(crate) fn ed25519_scalar_encoding_precheck(encoded: &[u8]) -> bool {
    encoded.len() == 32 && encoded.iter().rev().lt(ED25519_SCALAR_ORDER.iter().rev())
}

pub fn hkdf_sha256(
    input_key_material: &[u8],
    salt: &[u8],
    info: &[u8],
    output_len: usize,
) -> Result<SecretBytes, CryptoError> {
    if !(1..=255 * 32).contains(&output_len) {
        return Err(CryptoError::InvalidOutputLength);
    }
    struct OutputLength(usize);
    impl hkdf::KeyType for OutputLength {
        fn len(&self) -> usize {
            self.0
        }
    }
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(input_key_material);
    let info = [info];
    let okm = prk
        .expand(&info, OutputLength(output_len))
        .map_err(|_| CryptoError::InvalidOutputLength)?;
    let mut output = Zeroizing::new(vec![0; output_len]);
    okm.fill(&mut output)
        .map_err(|_| CryptoError::InvalidOutputLength)?;
    Ok(SecretBytes(output))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;
