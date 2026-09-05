//! RFC 9180 HPKE, base mode only, one fixed suite: DHKEM(X25519, HKDF-SHA256)
//! with HKDF-SHA256 and AES-256-GCM. This is the recipient-envelope primitive
//! from RFC 0001 §7.1: it encrypts to a public key and nothing more.
//! Encryption to a key does not identify the sender, so every envelope that
//! crosses the network is additionally wrapped in the signed record
//! (`record.rs`).
//!
//! The construction is assembled from reviewed primitives (ring's HMAC/HKDF/
//! AES-GCM and curve25519-dalek's X25519 ladder); no curve or cipher code is
//! implemented here. Conformance is pinned against the RFC's A.1 vectors (the
//! KEM and key schedule are suite-independent; the AES-128 vector exercises
//! the identical code path with a narrower key).

use crate::{CryptoError, SecretBytes};
use curve25519_dalek::montgomery::MontgomeryPoint;
use ring::rand::SecureRandom;
use ring::{aead, hkdf, hmac};
use zeroize::Zeroizing;

const KEM_ID: u16 = 0x0020;
const KDF_ID: u16 = 0x0001;
const AEAD_AES_256_GCM: u16 = 0x0002;
#[cfg(test)]
const AEAD_AES_128_GCM: u16 = 0x0001;
const VERSION_LABEL: &[u8] = b"HPKE-v1";
const MODE_BASE: u8 = 0;
const NONCE_LEN: usize = 12;
/// Envelopes carry keyrings and object keys, never bulk content.
pub const MAX_PLAINTEXT_BYTES: usize = 64 * 1024;

/// X25519 private key. Debug output is redacted; bytes zeroize on drop.
pub struct HpkePrivateKey(Zeroizing<[u8; 32]>);

/// X25519 public key (32-byte u-coordinate).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HpkePublicKey([u8; 32]);

impl HpkePrivateKey {
    pub fn generate() -> Result<Self, CryptoError> {
        let mut bytes = Zeroizing::new([0; 32]);
        ring::rand::SystemRandom::new()
            .fill(bytes.as_mut())
            .map_err(|_| CryptoError::EntropyUnavailable)?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKeyLength)?;
        Ok(Self(Zeroizing::new(bytes)))
    }

    pub fn public_key(&self) -> HpkePublicKey {
        HpkePublicKey(MontgomeryPoint::mul_base_clamped(*self.0).to_bytes())
    }

    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }

    fn diffie_hellman(&self, peer: &HpkePublicKey) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
        let shared = Zeroizing::new(MontgomeryPoint(peer.0).mul_clamped(*self.0).to_bytes());
        // RFC 7748 §6.1 / RFC 9180 §4.1: a low-order peer point yields the
        // all-zero output and MUST be rejected.
        if shared.iter().all(|byte| *byte == 0) {
            return Err(CryptoError::InvalidPublicKey);
        }
        Ok(shared)
    }
}

impl std::fmt::Debug for HpkePrivateKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HpkePrivateKey([REDACTED])")
    }
}

impl HpkePublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKeyLength)?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A single-shot base-mode ciphertext plus its encapsulated key.
#[derive(Clone, PartialEq, Eq)]
pub struct HpkeSealed {
    pub enc: [u8; 32],
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for HpkeSealed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HpkeSealed([REDACTED])")
    }
}

/// Encrypt `plaintext` to `recipient` (RFC 9180 §6.1 single-shot, seq 0).
pub fn seal(
    recipient: &HpkePublicKey,
    info: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    max_plaintext_bytes: usize,
) -> Result<HpkeSealed, CryptoError> {
    let ephemeral = HpkePrivateKey::generate()?;
    seal_with_ephemeral(
        &ephemeral,
        recipient,
        info,
        aad,
        plaintext,
        max_plaintext_bytes,
    )
}

fn seal_with_ephemeral(
    ephemeral: &HpkePrivateKey,
    recipient: &HpkePublicKey,
    info: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    max_plaintext_bytes: usize,
) -> Result<HpkeSealed, CryptoError> {
    if max_plaintext_bytes > MAX_PLAINTEXT_BYTES || plaintext.len() > max_plaintext_bytes {
        return Err(CryptoError::SizeLimitExceeded);
    }
    let enc = ephemeral.public_key();
    let shared_secret = encapsulated_secret(ephemeral.diffie_hellman(recipient)?, &enc, recipient)?;
    let schedule = key_schedule(&aead::AES_256_GCM, AEAD_AES_256_GCM, &shared_secret, info)?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, schedule.key.as_bytes())
            .map_err(|_| CryptoError::InvalidKeyLength)?,
    );
    let mut buffer = Vec::with_capacity(plaintext.len() + aead::AES_256_GCM.tag_len());
    buffer.extend_from_slice(plaintext);
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(schedule.base_nonce),
        aead::Aad::from(aad),
        &mut buffer,
    )
    .map_err(|_| CryptoError::AuthenticationFailed)?;
    Ok(HpkeSealed {
        enc: enc.0,
        ciphertext: buffer,
    })
}

/// Decrypt a single-shot base-mode ciphertext addressed to `recipient`.
pub fn open(
    recipient: &HpkePrivateKey,
    enc: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
    max_plaintext_bytes: usize,
) -> Result<SecretBytes, CryptoError> {
    if max_plaintext_bytes > MAX_PLAINTEXT_BYTES {
        return Err(CryptoError::SizeLimitExceeded);
    }
    let ephemeral_public = HpkePublicKey(*enc);
    let shared_secret = encapsulated_secret(
        recipient.diffie_hellman(&ephemeral_public)?,
        &ephemeral_public,
        &recipient.public_key(),
    )?;
    let schedule = key_schedule(&aead::AES_256_GCM, AEAD_AES_256_GCM, &shared_secret, info)?;
    crate::open_aes256_gcm(
        schedule.key.as_bytes(),
        &schedule.base_nonce,
        aad,
        ciphertext,
        max_plaintext_bytes,
    )
}

struct Schedule {
    key: SecretBytes,
    base_nonce: [u8; NONCE_LEN],
}

fn kem_suite_id() -> [u8; 5] {
    let mut id = *b"KEM\0\0";
    id[3..].copy_from_slice(&KEM_ID.to_be_bytes());
    id
}

fn hpke_suite_id(aead_id: u16) -> [u8; 10] {
    let mut id = *b"HPKE\0\0\0\0\0\0";
    id[4..6].copy_from_slice(&KEM_ID.to_be_bytes());
    id[6..8].copy_from_slice(&KDF_ID.to_be_bytes());
    id[8..].copy_from_slice(&aead_id.to_be_bytes());
    id
}

/// RFC 9180 §4: `LabeledExtract(salt, label, ikm)`; the PRK is returned as raw
/// HMAC output because the key-schedule context embeds two of them.
fn labeled_extract(suite_id: &[u8], salt: &[u8], label: &[u8], ikm: &[u8]) -> Zeroizing<[u8; 32]> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, salt);
    let mut context = hmac::Context::with_key(&key);
    context.update(VERSION_LABEL);
    context.update(suite_id);
    context.update(label);
    context.update(ikm);
    let tag = context.sign();
    let mut prk = Zeroizing::new([0; 32]);
    prk.copy_from_slice(tag.as_ref());
    prk
}

/// RFC 9180 §4: `LabeledExpand(prk, label, info, L)`.
fn labeled_expand(
    suite_id: &[u8],
    prk: &[u8; 32],
    label: &[u8],
    info: &[u8],
    length: usize,
) -> Result<SecretBytes, CryptoError> {
    struct Length(usize);
    impl hkdf::KeyType for Length {
        fn len(&self) -> usize {
            self.0
        }
    }
    let length_prefix = u16::try_from(length)
        .map_err(|_| CryptoError::InvalidOutputLength)?
        .to_be_bytes();
    let prk = hkdf::Prk::new_less_safe(hkdf::HKDF_SHA256, prk);
    let parts: [&[u8]; 5] = [&length_prefix, VERSION_LABEL, suite_id, label, info];
    let okm = prk
        .expand(&parts, Length(length))
        .map_err(|_| CryptoError::InvalidOutputLength)?;
    let mut output = Zeroizing::new(vec![0; length]);
    okm.fill(&mut output)
        .map_err(|_| CryptoError::InvalidOutputLength)?;
    Ok(SecretBytes::new(output))
}

/// RFC 9180 §4.1 `ExtractAndExpand(dh, kem_context)` for DHKEM(X25519).
fn encapsulated_secret(
    dh: Zeroizing<[u8; 32]>,
    enc: &HpkePublicKey,
    recipient: &HpkePublicKey,
) -> Result<SecretBytes, CryptoError> {
    let suite = kem_suite_id();
    let eae_prk = labeled_extract(&suite, &[], b"eae_prk", dh.as_ref());
    let mut kem_context = [0; 64];
    kem_context[..32].copy_from_slice(&enc.0);
    kem_context[32..].copy_from_slice(&recipient.0);
    labeled_expand(&suite, &eae_prk, b"shared_secret", &kem_context, 32)
}

/// RFC 9180 §5.1 `KeySchedule` for base mode (no PSK).
fn key_schedule(
    algorithm: &'static aead::Algorithm,
    aead_id: u16,
    shared_secret: &SecretBytes,
    info: &[u8],
) -> Result<Schedule, CryptoError> {
    let suite = hpke_suite_id(aead_id);
    let psk_id_hash = labeled_extract(&suite, &[], b"psk_id_hash", &[]);
    let info_hash = labeled_extract(&suite, &[], b"info_hash", info);
    let mut context = Vec::with_capacity(65);
    context.push(MODE_BASE);
    context.extend_from_slice(psk_id_hash.as_ref());
    context.extend_from_slice(info_hash.as_ref());
    let secret = labeled_extract(&suite, shared_secret.as_bytes(), b"secret", &[]);
    let key = labeled_expand(&suite, &secret, b"key", &context, algorithm.key_len())?;
    let nonce = labeled_expand(&suite, &secret, b"base_nonce", &context, NONCE_LEN)?;
    let mut base_nonce = [0; NONCE_LEN];
    base_nonce.copy_from_slice(nonce.as_bytes());
    Ok(Schedule { key, base_nonce })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    /// RFC 9180 Appendix A.1.1: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
    /// AES-128-GCM, base mode. The KEM and key schedule are the exact code
    /// used by the production AES-256 suite; only the AEAD width differs.
    #[test]
    fn matches_rfc9180_a1_base_vectors() {
        let sk_e = HpkePrivateKey::from_bytes(&hex(
            "52c4a758a802cd8b936eceea314432798d5baf2d7e9235dc084ab1b9cfa2f736",
        ))
        .unwrap();
        let sk_r = HpkePrivateKey::from_bytes(&hex(
            "4612c550263fc8ad58375df3f557aac531d26850903e55a9f23f21d8534e8ac8",
        ))
        .unwrap();
        let pk_r = sk_r.public_key();
        assert_eq!(
            pk_r.as_bytes().to_vec(),
            hex("3948cfe0ad1ddb695d780e59077195da6c56506b027329794ab02bca80815c4d")
        );
        let enc = sk_e.public_key();
        assert_eq!(
            enc.as_bytes().to_vec(),
            hex("37fda3567bdbd628e88668c3c8d7e97d1d1253b6d4ea6d44c150f741f1bf4431")
        );
        let shared = encapsulated_secret(sk_e.diffie_hellman(&pk_r).unwrap(), &enc, &pk_r).unwrap();
        assert_eq!(
            shared.as_bytes().to_vec(),
            hex("fe0e18c9f024ce43799ae393c7e8fe8fce9d218875e8227b0187c04e7d2ea1fc")
        );
        // Decapsulation on the recipient side reaches the same secret.
        let decapsulated =
            encapsulated_secret(sk_r.diffie_hellman(&enc).unwrap(), &enc, &pk_r).unwrap();
        assert_eq!(decapsulated.as_bytes(), shared.as_bytes());

        let info = hex("4f6465206f6e2061204772656369616e2055726e");
        let schedule = key_schedule(&aead::AES_128_GCM, AEAD_AES_128_GCM, &shared, &info).unwrap();
        assert_eq!(
            schedule.key.as_bytes().to_vec(),
            hex("4531685d41d65f03dc48f6b8302c05b0")
        );
        assert_eq!(
            schedule.base_nonce.to_vec(),
            hex("56d890e5accaaf011cff4b7d")
        );

        // Sequence-0 encryption with the derived key and nonce.
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, schedule.key.as_bytes()).unwrap(),
        );
        let mut buffer = hex("4265617574792069732074727574682c20747275746820626561757479");
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(schedule.base_nonce),
            aead::Aad::from(hex("436f756e742d30")),
            &mut buffer,
        )
        .unwrap();
        assert_eq!(
            buffer,
            hex(
                "f938558b5d72f1a23810b4be2ab4f84331acc02fc97babc53a52ae8218a355a96d8770ac83d07bea87e13c512a"
            )
        );
    }

    #[test]
    fn round_trips_and_binds_info_aad_and_recipient() {
        let recipient = HpkePrivateKey::generate().unwrap();
        let other = HpkePrivateKey::generate().unwrap();
        let sealed = seal(
            &recipient.public_key(),
            b"info",
            b"aad",
            b"keyring bytes",
            1024,
        )
        .unwrap();
        let opened = open(
            &recipient,
            &sealed.enc,
            b"info",
            b"aad",
            &sealed.ciphertext,
            1024,
        )
        .unwrap();
        assert_eq!(opened.as_bytes(), b"keyring bytes");
        assert!(
            open(
                &other,
                &sealed.enc,
                b"info",
                b"aad",
                &sealed.ciphertext,
                1024
            )
            .is_err()
        );
        assert!(
            open(
                &recipient,
                &sealed.enc,
                b"INFO",
                b"aad",
                &sealed.ciphertext,
                1024
            )
            .is_err()
        );
        assert!(
            open(
                &recipient,
                &sealed.enc,
                b"info",
                b"AAD",
                &sealed.ciphertext,
                1024
            )
            .is_err()
        );
        let mut tampered = sealed.ciphertext.clone();
        tampered[0] ^= 1;
        assert!(open(&recipient, &sealed.enc, b"info", b"aad", &tampered, 1024).is_err());
        assert!(matches!(
            open(
                &recipient,
                &sealed.enc,
                b"info",
                b"aad",
                &sealed.ciphertext,
                4
            ),
            Err(CryptoError::SizeLimitExceeded)
        ));
        // Two seals of one plaintext never share an encapsulation or bytes.
        let again = seal(
            &recipient.public_key(),
            b"info",
            b"aad",
            b"keyring bytes",
            1024,
        )
        .unwrap();
        assert_ne!(again.enc, sealed.enc);
        assert_ne!(again.ciphertext, sealed.ciphertext);
    }

    #[test]
    fn rejects_low_order_peer_points_and_oversized_input() {
        let recipient = HpkePrivateKey::generate().unwrap();
        let zero_point = HpkePublicKey([0; 32]);
        assert!(matches!(
            seal(&zero_point, b"", b"", b"x", 16),
            Err(CryptoError::InvalidPublicKey)
        ));
        let sealed = seal(&recipient.public_key(), b"", b"", b"x", 16).unwrap();
        assert!(matches!(
            open(&recipient, &[0; 32], b"", b"", &sealed.ciphertext, 16),
            Err(CryptoError::InvalidPublicKey)
        ));
        assert!(matches!(
            seal(&recipient.public_key(), b"", b"", &[0; 17], 16),
            Err(CryptoError::SizeLimitExceeded)
        ));
        assert!(matches!(
            seal(
                &recipient.public_key(),
                b"",
                b"",
                b"",
                MAX_PLAINTEXT_BYTES + 1
            ),
            Err(CryptoError::SizeLimitExceeded)
        ));
        assert!(HpkePrivateKey::from_bytes(&[1; 31]).is_err());
        assert_eq!(format!("{recipient:?}"), "HpkePrivateKey([REDACTED])");
    }
}
