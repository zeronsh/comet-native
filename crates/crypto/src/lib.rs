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
    signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(message, signature)
        .map_err(|_| CryptoError::AuthenticationFailed)
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
