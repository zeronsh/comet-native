use crate::record::{
    RecordBinding, RecordError, RecordKind, UnverifiedRecord, encode_signed, signing_bytes,
};
use crate::tests::hex;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixtures {
    signed_records: Vec<Fixture>,
    record_mutations: Vec<Mutation>,
}

#[derive(Deserialize)]
struct Mutation {
    name: String,
    offset: usize,
    remove: usize,
    insert: String,
    error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    name: String,
    kind: u64,
    vault_id: String,
    generation: String,
    epoch: u64,
    object_id: String,
    author_id: String,
    revision_id: String,
    membership_hash: String,
    payload: String,
    seed: String,
    public_key: String,
    signing_bytes: String,
    signature: String,
    peer_signature: Option<String>,
    peer_record: Option<String>,
}

fn binding() -> RecordBinding {
    RecordBinding {
        kind: RecordKind::Content,
        vault_id: [1; 16],
        generation: [2; 16],
        epoch: 24,
        object_id: [3; 16],
        author_id: [4; 16],
        membership_hash: [6; 32],
    }
}

fn signed(binding: &RecordBinding, revision: &[u8; 16], payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let signer = Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap();
    let input = signing_bytes(binding, revision, payload, payload.len()).unwrap();
    let signature: [u8; 64] = signer.sign(&input).as_ref().try_into().unwrap();
    (
        encode_signed(binding, revision, payload, &signature, payload.len()).unwrap(),
        signer.public_key().as_ref().to_vec(),
    )
}

#[test]
fn signed_record_rejects_identity_signer() {
    let context = binding();
    let mut identity = [0; 32];
    identity[0] = 1;
    let mut signature = [0; 64];
    signature[0] = 1;
    let encoded = encode_signed(&context, &[5; 16], &[], &signature, 0).unwrap();
    assert_eq!(
        UnverifiedRecord::parse(&encoded, 0)
            .unwrap()
            .verify(&context, &identity)
            .unwrap_err(),
        RecordError::InvalidSignature
    );
}

#[test]
fn signed_record_shared_fixture() {
    let live = std::env::var("ZERON_CRYPTO_TEST_VECTORS")
        .ok()
        .map(|path| std::fs::read_to_string(path).unwrap());
    let fixtures: Fixtures = serde_json::from_str(
        live.as_deref()
            .unwrap_or(include_str!("../tests/fixtures/primitives.json")),
    )
    .unwrap();
    assert!(!fixtures.signed_records.is_empty());
    for v in fixtures.signed_records {
        let binding = RecordBinding {
            kind: RecordKind::try_from(v.kind).unwrap(),
            vault_id: hex(&v.vault_id).try_into().unwrap(),
            generation: hex(&v.generation).try_into().unwrap(),
            epoch: v.epoch,
            object_id: hex(&v.object_id).try_into().unwrap(),
            author_id: hex(&v.author_id).try_into().unwrap(),
            membership_hash: hex(&v.membership_hash).try_into().unwrap(),
        };
        let revision: [u8; 16] = hex(&v.revision_id).try_into().unwrap();
        let payload = hex(&v.payload);
        let input = signing_bytes(&binding, &revision, &payload, payload.len()).unwrap();
        assert_eq!(input, hex(&v.signing_bytes), "{}", v.name);
        let signer = Ed25519KeyPair::from_seed_unchecked(&hex(&v.seed)).unwrap();
        assert_eq!(signer.public_key().as_ref(), hex(&v.public_key));
        let signature: [u8; 64] = signer.sign(&input).as_ref().try_into().unwrap();
        assert_eq!(signature.as_slice(), hex(&v.signature));
        let encoded =
            encode_signed(&binding, &revision, &payload, &signature, payload.len()).unwrap();
        let unverified = UnverifiedRecord::parse(&encoded, payload.len()).unwrap();
        assert_eq!(unverified.untrusted_binding(), &binding);
        assert_eq!(format!("{unverified:?}"), "UnverifiedRecord([REDACTED])");
        let verified = unverified.verify(&binding, &hex(&v.public_key)).unwrap();
        assert_eq!(verified.payload(), payload);
        assert_eq!(verified.binding(), &binding);
        assert_eq!(verified.revision_id(), &revision);
        assert_eq!(format!("{verified:?}"), "VerifiedRecord([REDACTED])");
        if live.is_some() {
            let peer_signature: [u8; 64] = hex(&v.peer_signature.unwrap()).try_into().unwrap();
            let peer_encoded = hex(&v.peer_record.unwrap());
            assert_eq!(
                peer_encoded,
                encode_signed(
                    &binding,
                    &revision,
                    &payload,
                    &peer_signature,
                    payload.len()
                )
                .unwrap()
            );
            let peer = UnverifiedRecord::parse(&peer_encoded, payload.len())
                .unwrap()
                .verify(&binding, &hex(&v.public_key))
                .unwrap();
            assert_eq!(peer.payload(), payload);
        }
    }
}

#[test]
fn signed_record_checks_every_trusted_binding_field() {
    let expected = binding();
    let (encoded, public_key) = signed(&expected, &[5; 16], &[0, 255, 16, 32]);
    let mut alternatives = Vec::new();
    let mut changed = expected;
    changed.kind = RecordKind::Policy;
    alternatives.push(changed);
    let mut changed = expected;
    changed.vault_id[0] ^= 1;
    alternatives.push(changed);
    let mut changed = expected;
    changed.generation[0] ^= 1;
    alternatives.push(changed);
    let mut changed = expected;
    changed.epoch += 1;
    alternatives.push(changed);
    let mut changed = expected;
    changed.object_id[0] ^= 1;
    alternatives.push(changed);
    let mut changed = expected;
    changed.author_id[0] ^= 1;
    alternatives.push(changed);
    let mut changed = expected;
    changed.membership_hash[0] ^= 1;
    alternatives.push(changed);
    for changed in alternatives {
        assert_eq!(
            UnverifiedRecord::parse(&encoded, 4)
                .unwrap()
                .verify(&changed, &public_key)
                .unwrap_err(),
            RecordError::ContextMismatch
        );
    }
    assert_eq!(
        UnverifiedRecord::parse(&encoded, 4)
            .unwrap()
            .verify(&expected, &[0; 32])
            .unwrap_err(),
        RecordError::InvalidSignature
    );
    assert_eq!(
        UnverifiedRecord::parse(&encoded, 4)
            .unwrap()
            .verify(&expected, &[0; 31])
            .unwrap_err(),
        RecordError::InvalidSignature
    );
}

#[test]
fn signed_record_authenticates_every_byte_and_preserves_input() {
    let expected = binding();
    let (encoded, public_key) = signed(&expected, &[5; 16], &[0, 255, 16, 32]);
    for index in 0..encoded.len() {
        let mut changed = encoded.clone();
        changed[index] ^= 1;
        let before = changed.clone();
        assert!(
            UnverifiedRecord::parse(&changed, 4)
                .and_then(|record| record.verify(&expected, &public_key))
                .is_err(),
            "byte {index}"
        );
        assert_eq!(changed, before);
    }
    for length in 0..encoded.len() {
        assert!(
            UnverifiedRecord::parse(&encoded[..length], 4).is_err(),
            "length {length}"
        );
    }
}

#[test]
fn signed_record_rejects_ambiguous_cbor() {
    let (encoded, _) = signed(&binding(), &[5; 16], &[0, 255, 16, 32]);
    let fixtures: Fixtures =
        serde_json::from_str(include_str!("../tests/fixtures/primitives.json")).unwrap();
    assert!(fixtures.record_mutations.len() >= 21);
    for mutation in fixtures.record_mutations {
        let mut changed = encoded.clone();
        changed.splice(
            mutation.offset..mutation.offset + mutation.remove,
            hex(&mutation.insert),
        );
        let error = UnverifiedRecord::parse(&changed, 4).unwrap_err();
        assert_eq!(format!("{error:?}"), mutation.error, "{}", mutation.name);
    }
    let mut trailing = encoded;
    trailing.push(0);
    assert_eq!(
        UnverifiedRecord::parse(&trailing, 4).unwrap_err(),
        RecordError::Malformed
    );
}

#[test]
fn signed_record_bounds_payload_and_limit_arithmetic() {
    let context = binding();
    let (encoded, _) = signed(&context, &[5; 16], &[0, 255, 16, 32]);
    assert_eq!(
        UnverifiedRecord::parse(&encoded, 3).unwrap_err(),
        RecordError::SizeLimitExceeded
    );
    assert_eq!(
        UnverifiedRecord::parse(&vec![0; 257], 0).unwrap_err(),
        RecordError::SizeLimitExceeded
    );
    assert_eq!(
        UnverifiedRecord::parse(&encoded, usize::MAX).unwrap_err(),
        RecordError::SizeLimitExceeded
    );
    assert_eq!(
        signing_bytes(&context, &[5; 16], &[0; 4], 3).unwrap_err(),
        RecordError::SizeLimitExceeded
    );
    assert_eq!(
        encode_signed(&context, &[5; 16], &[0; 4], &[0; 64], 3).unwrap_err(),
        RecordError::SizeLimitExceeded
    );
    let mut zero_epoch = context;
    zero_epoch.epoch = 0;
    assert_eq!(
        signing_bytes(&zero_epoch, &[5; 16], &[], 0).unwrap_err(),
        RecordError::InvalidEpoch
    );
}

#[test]
fn signed_record_round_trips_integer_and_length_boundaries() {
    for epoch in [
        1,
        23,
        24,
        255,
        256,
        65535,
        65536,
        u32::MAX as u64,
        1 << 32,
        u64::MAX,
    ] {
        let context = RecordBinding { epoch, ..binding() };
        for length in [0, 23, 24, 255, 256, 65535, 65536] {
            let payload = vec![42; length];
            let (encoded, key) = signed(&context, &[5; 16], &payload);
            let record = UnverifiedRecord::parse(&encoded, length)
                .unwrap()
                .verify(&context, &key)
                .unwrap();
            assert_eq!(record.payload(), payload);
            assert_eq!(record.binding().epoch, epoch);
        }
    }
}
