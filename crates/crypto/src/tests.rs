use super::*;
use ring::{aead, signature};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vectors {
    version: u32,
    aes256gcm: Vec<AesVector>,
    ed25519: Vec<SignatureVector>,
    hkdf_sha256: Vec<HkdfVector>,
    ed25519_point_encodings: Vec<EncodingVector>,
    ed25519_scalar_encodings: Vec<EncodingVector>,
    ed25519_rejections: Vec<RejectionVector>,
}

#[derive(Deserialize)]
struct EncodingVector {
    name: String,
    encoding: String,
    allowed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RejectionVector {
    name: String,
    public_key: String,
    message: String,
    signature: String,
}

#[derive(Deserialize)]
struct AesVector {
    name: String,
    key: String,
    nonce: String,
    aad: String,
    plaintext: String,
    ciphertext: String,
    tag: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureVector {
    name: String,
    seed: String,
    public_key: String,
    message: String,
    signature: String,
    peer_signature: Option<String>,
}

#[derive(Deserialize)]
struct HkdfVector {
    name: String,
    ikm: String,
    salt: String,
    info: String,
    output: String,
}

fn vectors() -> Vectors {
    let fixture = std::env::var("ZERON_CRYPTO_TEST_VECTORS")
        .ok()
        .map(|path| std::fs::read_to_string(path).unwrap());
    let vectors: Vectors = serde_json::from_str(
        fixture
            .as_deref()
            .unwrap_or(include_str!("../tests/fixtures/primitives.json")),
    )
    .unwrap();
    assert_eq!(vectors.version, 1);
    assert!(
        !vectors.aes256gcm.is_empty()
            && !vectors.ed25519.is_empty()
            && !vectors.hkdf_sha256.is_empty()
    );
    if fixture.is_some() {
        assert!(vectors.ed25519.iter().all(|v| v.peer_signature.is_some()));
    }
    vectors
}

pub(super) fn hex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn aes_known_answers_match_published_and_swift_vectors() {
    for v in vectors().aes256gcm {
        let key = hex(&v.key);
        let nonce = hex(&v.nonce);
        let aad = hex(&v.aad);
        let plaintext = hex(&v.plaintext);
        let ciphertext = hex(&(v.ciphertext + &v.tag));
        let opened = open_aes256_gcm(&key, &nonce, &aad, &ciphertext, plaintext.len()).unwrap();
        assert_eq!(opened.as_bytes(), plaintext, "{}", v.name);
        let mut sealed = plaintext;
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM, &key).unwrap());
        key.seal_in_place_append_tag(
            aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
            aead::Aad::from(&aad),
            &mut sealed,
        )
        .unwrap();
        assert_eq!(sealed, ciphertext, "{}", v.name);
    }
}

#[test]
fn aes_rejects_tampering_without_modifying_the_input() {
    let v = vectors().aes256gcm.remove(1);
    let key = hex(&v.key);
    let nonce = hex(&v.nonce);
    let ciphertext = hex(&(v.ciphertext + &v.tag));
    for index in 0..ciphertext.len() {
        let mut damaged = ciphertext.clone();
        damaged[index] ^= 1;
        let before = damaged.clone();
        assert_eq!(
            open_aes256_gcm(&key, &nonce, &[], &damaged, 16).unwrap_err(),
            CryptoError::AuthenticationFailed
        );
        assert_eq!(damaged, before);
    }
    assert_eq!(
        open_aes256_gcm(&key, &nonce, &[1], &ciphertext, 16).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
    assert_eq!(
        open_aes256_gcm(&[1; 32], &nonce, &[], &ciphertext, 16).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
    assert_eq!(
        open_aes256_gcm(&key, &[1; 12], &[], &ciphertext, 16).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
}

#[test]
fn aes_validates_lengths_and_budget_before_opening() {
    let v = vectors().aes256gcm.remove(1);
    let key = hex(&v.key);
    let nonce = hex(&v.nonce);
    let ciphertext = hex(&(v.ciphertext + &v.tag));
    for length in [0, 16, 24, 31, 33] {
        assert_eq!(
            open_aes256_gcm(&vec![0; length], &nonce, &[], &ciphertext, 16).unwrap_err(),
            CryptoError::InvalidKeyLength
        );
    }
    for length in [0, 8, 11, 13, 16] {
        assert_eq!(
            open_aes256_gcm(&key, &vec![0; length], &[], &ciphertext, 16).unwrap_err(),
            CryptoError::InvalidNonceLength
        );
    }
    for length in 0..16 {
        assert_eq!(
            open_aes256_gcm(&key, &nonce, &[], &ciphertext[..length], 16).unwrap_err(),
            CryptoError::InvalidCiphertextLength
        );
    }
    assert_eq!(
        open_aes256_gcm(&key, &nonce, &[], &ciphertext, 15).unwrap_err(),
        CryptoError::SizeLimitExceeded
    );
}

#[test]
fn ed25519_known_answers_match_published_and_swift_vectors() {
    use signature::KeyPair;
    for v in vectors().ed25519 {
        let key = hex(&v.public_key);
        let message = hex(&v.message);
        let sig = hex(&v.signature);
        verify_ed25519(&key, &message, &sig).unwrap();
        if let Some(peer_signature) = &v.peer_signature {
            verify_ed25519(&key, &message, &hex(peer_signature)).unwrap();
        }
        let signer = signature::Ed25519KeyPair::from_seed_unchecked(&hex(&v.seed)).unwrap();
        assert_eq!(signer.public_key().as_ref(), key, "{}", v.name);
        assert_eq!(signer.sign(&message).as_ref(), sig, "{}", v.name);
        let mut changed = message;
        changed.push(0);
        assert_eq!(
            verify_ed25519(&key, &changed, &sig),
            Err(CryptoError::AuthenticationFailed)
        );
        for index in 0..sig.len() {
            let mut damaged = sig.clone();
            damaged[index] ^= 1;
            assert_eq!(
                verify_ed25519(&key, &hex(&v.message), &damaged),
                Err(CryptoError::AuthenticationFailed)
            );
        }
    }
}

#[test]
fn ed25519_rejects_invalid_lengths_and_wrong_keys() {
    let v = vectors().ed25519.remove(0);
    let key = hex(&v.public_key);
    let sig = hex(&v.signature);
    for length in [0, 31, 33] {
        assert_eq!(
            verify_ed25519(&vec![0; length], &[], &sig),
            Err(CryptoError::InvalidKeyLength)
        );
    }
    for length in [0, 63, 65] {
        assert_eq!(
            verify_ed25519(&key, &[], &vec![0; length]),
            Err(CryptoError::InvalidSignatureLength)
        );
    }
    assert_eq!(
        verify_ed25519(&[0; 32], &[], &sig),
        Err(CryptoError::AuthenticationFailed)
    );
}

#[test]
fn ed25519_encoding_prechecks_match_shared_vectors() {
    let fixtures = vectors();
    assert!(fixtures.ed25519_point_encodings.len() >= 13);
    assert!(fixtures.ed25519_scalar_encodings.len() >= 9);
    for v in fixtures.ed25519_point_encodings {
        let mut encoded = hex(&v.encoding);
        let before = encoded.clone();
        assert_eq!(
            ed25519_point_encoding_precheck(&encoded),
            v.allowed,
            "{}",
            v.name
        );
        assert_eq!(encoded, before);
        if encoded.len() == 32 {
            encoded[31] ^= 0x80;
            assert_eq!(
                ed25519_point_encoding_precheck(&encoded),
                v.allowed,
                "{} opposite sign",
                v.name
            );
        }
    }
    for v in fixtures.ed25519_scalar_encodings {
        let encoded = hex(&v.encoding);
        assert_eq!(
            ed25519_scalar_encoding_precheck(&encoded),
            v.allowed,
            "{}",
            v.name
        );
    }
    for low_byte in 0xed..=0xff {
        for high_byte in [0x7f, 0xff] {
            let mut noncanonical = [0xff; 32];
            noncanonical[0] = low_byte;
            noncanonical[31] = high_byte;
            assert!(!ed25519_point_encoding_precheck(&noncanonical));
        }
    }
}

#[test]
fn ed25519_rejection_vectors_fail_closed() {
    let fixtures = vectors();
    assert!(fixtures.ed25519_rejections.len() >= 8);
    for v in fixtures.ed25519_rejections {
        let key = hex(&v.public_key);
        let message = hex(&v.message);
        let sig = hex(&v.signature);
        assert_eq!(key.len(), 32);
        assert_eq!(sig.len(), 64);
        assert_eq!(
            verify_ed25519(&key, &message, &sig),
            Err(CryptoError::AuthenticationFailed),
            "{}",
            v.name
        );
    }
}

#[test]
fn ed25519_rejects_identity_key_signature() {
    let mut identity = [0; 32];
    identity[0] = 1;
    let mut signature = [0; 64];
    signature[0] = 1;
    assert_eq!(
        verify_ed25519(&identity, b"synthetic key-admission probe", &signature),
        Err(CryptoError::AuthenticationFailed)
    );
}

#[test]
fn hkdf_matches_rfc5869_and_separates_labels() {
    for v in vectors().hkdf_sha256 {
        let ikm = hex(&v.ikm);
        let salt = hex(&v.salt);
        let info = hex(&v.info);
        let output = hex(&v.output);
        let derived = hkdf_sha256(&ikm, &salt, &info, output.len()).unwrap();
        assert_eq!(derived.as_bytes(), output, "{}", v.name);
        let mut changed_info = info;
        changed_info.push(0);
        assert_ne!(
            hkdf_sha256(&ikm, &salt, &changed_info, output.len())
                .unwrap()
                .as_bytes(),
            output
        );
        assert_eq!(format!("{derived:?}"), "SecretBytes([REDACTED])");
    }
}

#[test]
fn hkdf_bounds_output_before_allocation() {
    for length in [0, 8161, usize::MAX] {
        assert_eq!(
            hkdf_sha256(&[1; 32], &[], &[], length).unwrap_err(),
            CryptoError::InvalidOutputLength
        );
    }
    for length in [1, 32, 8160] {
        assert_eq!(
            hkdf_sha256(&[1; 32], &[], &[], length)
                .unwrap()
                .as_bytes()
                .len(),
            length
        );
    }
}
