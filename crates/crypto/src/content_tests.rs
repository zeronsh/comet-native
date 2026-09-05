use super::*;
use crate::record::{RecordBinding, RecordKind};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn binding() -> RecordBinding {
    RecordBinding {
        kind: RecordKind::Content,
        vault_id: [1; 16],
        generation: [2; 16],
        epoch: 1,
        object_id: [3; 16],
        author_id: [4; 16],
        membership_hash: [5; 32],
    }
}

fn keys() -> Result<(ContentKey, DeviceSigner), ContentError> {
    Ok((
        ContentKey::from_bytes(KeyScope::from(&binding()), [6; 16], &[7; 32])?,
        DeviceSigner::from_seed([4; 16], &[8; 32])?,
    ))
}

#[test]
fn content_round_trip_and_immutable_retry_bytes() -> TestResult {
    let (key, signer) = keys()?;
    let plaintext = b"private transcript\0\xff";
    let sealed = seal(
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        plaintext,
        1024,
    )?;
    let retry = sealed.encoded().to_vec();
    let opened = open(
        sealed.encoded(),
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        signer.public_key(),
        1024,
    )?;
    assert_eq!(opened.plaintext().as_bytes(), plaintext);
    assert_eq!(opened.revision_id(), sealed.revision_id());
    assert_eq!(retry, sealed.encoded());
    assert!(
        !sealed
            .encoded()
            .windows(plaintext.len())
            .any(|window| window == plaintext)
    );
    assert_eq!(format!("{key:?}"), "ContentKey([REDACTED])");
    assert_eq!(format!("{signer:?}"), "DeviceSigner([REDACTED])");
    assert_eq!(format!("{opened:?}"), "OpenedContent([REDACTED])");
    Ok(())
}

#[test]
fn content_fresh_seals_are_distinct_and_wrong_keys_fail() -> TestResult {
    let (key, signer) = keys()?;
    let first = seal(
        &binding(),
        ContentPurpose::Tail,
        &key,
        &signer,
        b"same content",
        1024,
    )?;
    let second = seal(
        &binding(),
        ContentPurpose::Tail,
        &key,
        &signer,
        b"same content",
        1024,
    )?;
    assert_ne!(first.revision_id(), second.revision_id());
    assert_ne!(first.encoded(), second.encoded());
    let wrong_key = ContentKey::from_bytes(KeyScope::from(&binding()), [6; 16], &[9; 32])?;
    assert!(
        open(
            first.encoded(),
            &binding(),
            ContentPurpose::Tail,
            &wrong_key,
            signer.public_key(),
            1024
        )
        .is_err()
    );
    let wrong_id = ContentKey::from_bytes(KeyScope::from(&binding()), [9; 16], &[7; 32])?;
    assert_eq!(
        open(
            first.encoded(),
            &binding(),
            ContentPurpose::Tail,
            &wrong_id,
            signer.public_key(),
            1024
        )
        .err(),
        Some(ContentError::WrongKey)
    );
    assert_eq!(
        open(
            first.encoded(),
            &binding(),
            ContentPurpose::Blob,
            &key,
            signer.public_key(),
            1024
        )
        .err(),
        Some(ContentError::WrongPurpose)
    );
    Ok(())
}

#[test]
fn content_rejects_tampering_and_truncation() -> TestResult {
    let (key, signer) = keys()?;
    let sealed = seal(
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"canary",
        128,
    )?;
    for index in 0..sealed.encoded().len() {
        let mut changed = sealed.encoded().to_vec();
        if let Some(byte) = changed.get_mut(index) {
            *byte ^= 1;
        }
        assert!(
            open(
                &changed,
                &binding(),
                ContentPurpose::ChatUpdate,
                &key,
                signer.public_key(),
                128
            )
            .is_err()
        );
    }
    for length in 0..sealed.encoded().len() {
        let truncated = sealed.encoded().get(..length).ok_or("invalid test range")?;
        assert!(
            open(
                truncated,
                &binding(),
                ContentPurpose::ChatUpdate,
                &key,
                signer.public_key(),
                128
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn content_authenticates_ciphertext_and_context_even_after_resigning() -> TestResult {
    let (key, signer) = keys()?;
    let sealed = seal(
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"private",
        128,
    )?;
    let verified = UnverifiedRecord::parse(sealed.encoded(), payload_limit(128)?)?
        .verify(&binding(), signer.public_key())?;
    let mut damaged = verified.payload().to_vec();
    *damaged.last_mut().ok_or("empty encrypted payload")? ^= 1;
    let input = record::signing_bytes(
        &binding(),
        sealed.revision_id(),
        &damaged,
        payload_limit(128)?,
    )?;
    let signature: [u8; 64] = signer.key_pair.sign(&input).as_ref().try_into()?;
    let encoded = record::encode_signed(
        &binding(),
        sealed.revision_id(),
        &damaged,
        &signature,
        payload_limit(128)?,
    )?;
    assert_eq!(
        open(
            &encoded,
            &binding(),
            ContentPurpose::ChatUpdate,
            &key,
            signer.public_key(),
            128
        )
        .err(),
        Some(ContentError::Crypto(CryptoError::AuthenticationFailed))
    );
    let mut changed_context = binding();
    changed_context.membership_hash = [9; 32];
    let input = record::signing_bytes(
        &changed_context,
        sealed.revision_id(),
        verified.payload(),
        payload_limit(128)?,
    )?;
    let signature: [u8; 64] = signer.key_pair.sign(&input).as_ref().try_into()?;
    let encoded = record::encode_signed(
        &changed_context,
        sealed.revision_id(),
        verified.payload(),
        &signature,
        payload_limit(128)?,
    )?;
    assert!(
        open(
            &encoded,
            &changed_context,
            ContentPurpose::ChatUpdate,
            &key,
            signer.public_key(),
            128
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn content_key_generation_and_empty_payloads() -> TestResult {
    let (key, signer) = keys()?;
    let generated = ContentKey::generate(KeyScope::from(&binding()))?;
    let another = ContentKey::generate(KeyScope::from(&binding()))?;
    assert_ne!(generated.identifier(), another.identifier());
    assert_ne!(generated.expose_secret(), another.expose_secret());
    for purpose in 1..=8 {
        let purpose = ContentPurpose::try_from(purpose)?;
        let sealed = seal(&binding(), purpose, &key, &signer, &[], 0)?;
        assert!(
            open(
                sealed.encoded(),
                &binding(),
                purpose,
                &key,
                signer.public_key(),
                0
            )?
            .plaintext()
            .as_bytes()
            .is_empty()
        );
    }
    assert!(ContentKey::from_bytes(KeyScope::from(&binding()), [6; 16], &[7; 31]).is_err());
    Ok(())
}

#[test]
fn content_shared_fixture_and_swift_records() -> TestResult {
    use crate::tests::hex;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixtures {
        encrypted_content: Vec<Fixture>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        name: String,
        vault_id: String,
        generation: String,
        epoch: u64,
        object_id: String,
        author_id: String,
        membership_hash: String,
        purpose: u64,
        key_id: String,
        content_key: String,
        signer_seed: String,
        public_key: String,
        plaintext: String,
        encoded: String,
        peer_record: Option<String>,
    }
    let live = std::env::var("ZERON_CRYPTO_TEST_VECTORS")
        .ok()
        .map(std::fs::read_to_string)
        .transpose()?;
    let fixtures: Fixtures = serde_json::from_str(
        live.as_deref()
            .unwrap_or(include_str!("../tests/fixtures/primitives.json")),
    )?;
    assert!(!fixtures.encrypted_content.is_empty());
    for fixture in fixtures.encrypted_content {
        let context = RecordBinding {
            kind: RecordKind::Content,
            vault_id: hex(&fixture.vault_id).as_slice().try_into()?,
            generation: hex(&fixture.generation).as_slice().try_into()?,
            epoch: fixture.epoch,
            object_id: hex(&fixture.object_id).as_slice().try_into()?,
            author_id: hex(&fixture.author_id).as_slice().try_into()?,
            membership_hash: hex(&fixture.membership_hash).as_slice().try_into()?,
        };
        let key = ContentKey::from_bytes(
            KeyScope::from(&context),
            hex(&fixture.key_id).as_slice().try_into()?,
            &hex(&fixture.content_key),
        )?;
        let signer = DeviceSigner::from_seed(context.author_id, &hex(&fixture.signer_seed))?;
        assert_eq!(signer.public_key(), hex(&fixture.public_key));
        let purpose = ContentPurpose::try_from(fixture.purpose)?;
        let plaintext = hex(&fixture.plaintext);
        let encoded = hex(&fixture.encoded);
        let sealed = seal_with_random(
            &context,
            purpose,
            &key,
            &signer,
            &plaintext,
            plaintext.len(),
            |material| {
                for (byte, value) in material.iter_mut().zip(0u8..) {
                    *byte = value;
                }
                Ok(())
            },
        )?;
        assert_eq!(sealed.encoded(), encoded, "{}", fixture.name);
        assert_eq!(
            open(
                &encoded,
                &context,
                purpose,
                &key,
                signer.public_key(),
                plaintext.len()
            )?
            .plaintext()
            .as_bytes(),
            plaintext
        );
        if live.is_some() {
            let peer_record = hex(fixture
                .peer_record
                .as_deref()
                .ok_or("missing Swift encrypted record")?);
            assert_eq!(
                open(
                    &peer_record,
                    &context,
                    purpose,
                    &key,
                    signer.public_key(),
                    plaintext.len()
                )?
                .plaintext()
                .as_bytes(),
                plaintext
            );
        }
    }
    Ok(())
}

#[test]
fn content_scope_and_author_are_checked_before_entropy() -> TestResult {
    let (key, signer) = keys()?;
    let mut entropy_calls = 0;
    let mut changed = binding();
    changed.epoch = 2;
    let result = seal_with_random(
        &changed,
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"x",
        1,
        |_| {
            entropy_calls += 1;
            Err(ContentError::EntropyUnavailable)
        },
    );
    assert_eq!(result.err(), Some(ContentError::WrongScope));
    changed = binding();
    changed.author_id = [9; 16];
    let result = seal_with_random(
        &changed,
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"x",
        1,
        |_| {
            entropy_calls += 1;
            Err(ContentError::EntropyUnavailable)
        },
    );
    assert_eq!(result.err(), Some(ContentError::WrongAuthor));
    assert_eq!(entropy_calls, 0);
    Ok(())
}

#[test]
fn content_entropy_and_size_failures_do_not_produce_a_record() -> TestResult {
    let (key, signer) = keys()?;
    let result = seal_with_random(
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"x",
        1,
        |_| Err(ContentError::EntropyUnavailable),
    );
    assert_eq!(result.err(), Some(ContentError::EntropyUnavailable));
    let mut entropy_calls = 0;
    let result = seal_with_random(
        &binding(),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        b"xx",
        1,
        |_| {
            entropy_calls += 1;
            Err(ContentError::EntropyUnavailable)
        },
    );
    assert_eq!(result.err(), Some(ContentError::SizeLimitExceeded));
    assert_eq!(entropy_calls, 0);
    assert!(
        seal(
            &binding(),
            ContentPurpose::ChatUpdate,
            &key,
            &signer,
            b"x",
            usize::MAX
        )
        .is_err()
    );
    Ok(())
}
