//! Shared vault control-plane fixture (`tests/fixtures/vault.json`): a
//! synthetic genesis → add device → revoke device history, keyring and
//! object-key envelopes, an enrollment proof, and one sealed chat record —
//! all under throwaway test keys. The edge (TypeScript) and native (Swift)
//! verifiers consume the same file, so every side authenticates identical
//! bytes. Regenerate with `ZERON_WRITE_VAULT_FIXTURE=1 cargo test -p
//! zeron-crypto --test vault_fixture`; the default run verifies the committed
//! file end to end.

use serde::{Deserialize, Serialize};
use zeron_crypto::content::{self, ContentKey, ContentPurpose, DeviceSigner, KeyScope};
use zeron_crypto::envelope::{self, RecipientKind};
use zeron_crypto::hpke::HpkePrivateKey;
use zeron_crypto::keyring::Keyring;
use zeron_crypto::policy::{
    self, DeviceEntry, DeviceStatus, EnrollmentRequest, MembershipState, Operation,
    POLICY_OBJECT_ID, PolicyPayload,
};
use zeron_crypto::recovery::RecoverySecret;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vault.json");

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceFixture {
    id: String,
    signing_seed: String,
    signing_key: String,
    encryption_secret: String,
    encryption_key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    vault_id: String,
    generation: String,
    org_id: String,
    user_id: String,
    profile_hash: String,
    recovery_secret: String,
    recovery_kit: String,
    recovery_signing_key: String,
    recovery_encryption_key: String,
    recovery_authority_id: String,
    device_a: DeviceFixture,
    device_b: DeviceFixture,
    /// Signed policy records in order (base64) and the membership hash after each.
    membership: Vec<String>,
    membership_hashes: Vec<String>,
    epochs_after: Vec<u64>,
    /// Keyring envelope to device B under the head after the add (epoch 1).
    keyring_envelope_b: String,
    keyring_epoch_1: String,
    object_id: String,
    object_key_envelope: String,
    object_key_id: String,
    object_key: String,
    chat_record: String,
    chat_plaintext: String,
    enrollment: EnrollmentFixture,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollmentFixture {
    request_id: String,
    device_id: String,
    signing_key: String,
    encryption_key: String,
    proof: String,
    pairing_code: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0; N];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap();
    }
    out
}

fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let symbols: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    for chunk in symbols.chunks(4) {
        let mut value = 0u32;
        for (index, symbol) in chunk.iter().enumerate() {
            let digit = ALPHABET.iter().position(|c| c == symbol).unwrap() as u32;
            value |= digit << (18 - 6 * index);
        }
        let bytes = value.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    out
}

struct Device {
    id: [u8; 16],
    seed: [u8; 32],
    signer: DeviceSigner,
    encryption: HpkePrivateKey,
}

impl Device {
    fn new(tag: u8) -> Self {
        let mut seed = [tag; 32];
        seed[0] ^= 0x5a;
        Self {
            id: [tag; 16],
            seed,
            signer: DeviceSigner::from_seed([tag; 16], &seed).unwrap(),
            encryption: HpkePrivateKey::from_bytes(&[tag ^ 0x33; 32]).unwrap(),
        }
    }

    fn entry(&self) -> DeviceEntry {
        DeviceEntry {
            device_id: self.id,
            signing_key: self.signer.public_key().try_into().unwrap(),
            encryption_key: *self.encryption.public_key().as_bytes(),
            status: DeviceStatus::Active,
        }
    }

    fn fixture(&self) -> DeviceFixture {
        DeviceFixture {
            id: hex(&self.id),
            signing_seed: hex(&self.seed),
            signing_key: hex(self.signer.public_key()),
            encryption_secret: hex(self.encryption.expose_secret()),
            encryption_key: hex(self.encryption.public_key().as_bytes()),
        }
    }
}

fn generate() -> Fixture {
    let vault_id = [0x11; 16];
    let generation = [0x22; 16];
    let profile = policy::profile_hash("org_fixture", "user_fixture");
    let recovery = RecoverySecret::from_bytes(&[0x44; 32]).unwrap();
    let a = Device::new(0xa1);
    let b = Device::new(0xb2);

    let genesis_payload = PolicyPayload {
        sequence: 0,
        parent_hash: [0; 32],
        profile_hash: profile,
        epoch: 1,
        operation: Operation::Genesis,
        recovery_signing_key: recovery.signing_public_key().unwrap(),
        recovery_encryption_key: *recovery.encryption_key().unwrap().public_key().as_bytes(),
        devices: vec![a.entry()],
    };
    let genesis = policy::encode_policy(
        &policy::policy_binding(vault_id, generation, 1, a.id, [0; 32]),
        &genesis_payload,
        &a.signer,
    )
    .unwrap();
    let state = MembershipState::from_genesis(&genesis, &vault_id, &generation, &profile).unwrap();

    let mut add = state.next_payload(Operation::AddDevice);
    add.devices.push(b.entry());
    let add_record = policy::encode_policy(
        &policy::policy_binding(vault_id, generation, add.epoch, a.id, *state.hash()),
        &add,
        &a.signer,
    )
    .unwrap();
    let added = state.apply(&add_record).unwrap();

    let mut keyring = Keyring::new();
    keyring.insert(1, &[0x66; 32]).unwrap();
    let keyring_envelope = envelope::seal_keyring(
        &added.envelope_binding(POLICY_OBJECT_ID, 1, a.id),
        RecipientKind::Device,
        &b.id,
        &b.encryption.public_key(),
        &keyring,
        &a.signer,
    )
    .unwrap();

    let object_id = [0x77; 16];
    let object_binding = added.envelope_binding(object_id, 1, a.id);
    let object_key =
        ContentKey::from_bytes(KeyScope::from(&object_binding), [0x88; 16], &[0x99; 32]).unwrap();
    let object_envelope = envelope::wrap_object_key(
        &object_binding,
        keyring.epoch_key(1).unwrap(),
        &object_key,
        &a.signer,
    )
    .unwrap();
    let chat_plaintext = b"fixture chat update: private canary text";
    let chat_record = content::seal(
        &added.content_binding(object_id, a.id),
        ContentPurpose::ChatUpdate,
        &object_key,
        &a.signer,
        chat_plaintext,
        1024,
    )
    .unwrap();

    let mut revoke = added.next_payload(Operation::RevokeDevice);
    revoke.devices[1].status = DeviceStatus::Revoked;
    let revoke_record = policy::encode_policy(
        &policy::policy_binding(vault_id, generation, revoke.epoch, a.id, *added.hash()),
        &revoke,
        &a.signer,
    )
    .unwrap();
    let revoked = added.apply(&revoke_record).unwrap();

    let pending = Device::new(0xc3);
    let request = EnrollmentRequest {
        vault_id,
        request_id: [0x55; 16],
        device_id: pending.id,
        signing_key: pending.signer.public_key().try_into().unwrap(),
        encryption_key: *pending.encryption.public_key().as_bytes(),
    };
    let proof = request.sign(&pending.signer).unwrap();

    Fixture {
        vault_id: hex(&vault_id),
        generation: hex(&generation),
        org_id: "org_fixture".into(),
        user_id: "user_fixture".into(),
        profile_hash: hex(&profile),
        recovery_secret: hex(recovery.expose_secret()),
        recovery_kit: recovery.to_kit().to_string(),
        recovery_signing_key: hex(&recovery.signing_public_key().unwrap()),
        recovery_encryption_key: hex(recovery.encryption_key().unwrap().public_key().as_bytes()),
        recovery_authority_id: hex(&state.recovery_authority_id()),
        device_a: a.fixture(),
        device_b: b.fixture(),
        membership: vec![b64(&genesis), b64(&add_record), b64(&revoke_record)],
        membership_hashes: vec![hex(state.hash()), hex(added.hash()), hex(revoked.hash())],
        epochs_after: vec![state.epoch(), added.epoch(), revoked.epoch()],
        keyring_envelope_b: b64(keyring_envelope.encoded()),
        keyring_epoch_1: hex(keyring.epoch_key(1).unwrap()),
        object_id: hex(&object_id),
        object_key_envelope: b64(object_envelope.encoded()),
        object_key_id: hex(object_key.identifier()),
        object_key: hex(object_key.expose_secret()),
        chat_record: b64(chat_record.encoded()),
        chat_plaintext: String::from_utf8(chat_plaintext.to_vec()).unwrap(),
        enrollment: EnrollmentFixture {
            request_id: hex(&request.request_id),
            device_id: hex(&request.device_id),
            signing_key: hex(&request.signing_key),
            encryption_key: hex(&request.encryption_key),
            proof: hex(&proof),
            pairing_code: request.pairing_code(state.genesis_hash()),
        },
    }
}

#[test]
fn vault_fixture_round_trips_across_the_control_plane() {
    if std::env::var_os("ZERON_WRITE_VAULT_FIXTURE").is_some() {
        let fixture = generate();
        let mut json = serde_json::to_string_pretty(&fixture).unwrap();
        json.push('\n');
        std::fs::write(FIXTURE, json).unwrap();
    }
    let fixture: Fixture =
        serde_json::from_str(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let vault_id = unhex::<16>(&fixture.vault_id);
    let generation = unhex::<16>(&fixture.generation);
    let profile = policy::profile_hash(&fixture.org_id, &fixture.user_id);
    assert_eq!(hex(&profile), fixture.profile_hash);

    // Membership chain verifies from the pinned genesis.
    let records: Vec<Vec<u8>> = fixture.membership.iter().map(|r| unb64(r)).collect();
    let mut state =
        MembershipState::from_genesis(&records[0], &vault_id, &generation, &profile).unwrap();
    assert_eq!(hex(state.hash()), fixture.membership_hashes[0]);
    for (index, record) in records.iter().enumerate().skip(1) {
        state = state.apply(record).unwrap();
        assert_eq!(hex(state.hash()), fixture.membership_hashes[index]);
        assert_eq!(state.epoch(), fixture.epochs_after[index]);
    }
    let a_id = unhex::<16>(&fixture.device_a.id);
    let b_id = unhex::<16>(&fixture.device_b.id);
    assert!(state.active_device(&a_id).is_some());
    assert!(state.active_device(&b_id).is_none());
    assert_eq!(
        hex(&state.recovery_authority_id()),
        fixture.recovery_authority_id
    );

    // Recovery kit text reproduces the secret and its derived public keys.
    let recovery = RecoverySecret::from_kit(&fixture.recovery_kit).unwrap();
    assert_eq!(hex(recovery.expose_secret()), fixture.recovery_secret);
    assert_eq!(
        hex(&recovery.signing_public_key().unwrap()),
        fixture.recovery_signing_key
    );

    // Device B opens its keyring envelope under the head that issued it.
    let added = MembershipState::from_genesis(&records[0], &vault_id, &generation, &profile)
        .unwrap()
        .apply(&records[1])
        .unwrap();
    let b_key =
        HpkePrivateKey::from_bytes(&unhex::<32>(&fixture.device_b.encryption_secret)).unwrap();
    let a_public = unhex::<32>(&fixture.device_a.signing_key);
    let keyring = envelope::open_keyring(
        &unb64(&fixture.keyring_envelope_b),
        &added.envelope_binding(POLICY_OBJECT_ID, 1, a_id),
        RecipientKind::Device,
        &b_id,
        &b_key,
        &a_public,
    )
    .unwrap();
    assert_eq!(hex(keyring.epoch_key(1).unwrap()), fixture.keyring_epoch_1);

    // The object key unwraps under epoch 1 and opens the chat record.
    let object_id = unhex::<16>(&fixture.object_id);
    let object_key = envelope::unwrap_object_key(
        &unb64(&fixture.object_key_envelope),
        &added.envelope_binding(object_id, 1, a_id),
        keyring.epoch_key(1).unwrap(),
        &a_public,
    )
    .unwrap();
    assert_eq!(hex(object_key.identifier()), fixture.object_key_id);
    let opened = content::open(
        &unb64(&fixture.chat_record),
        &added.content_binding(object_id, a_id),
        ContentPurpose::ChatUpdate,
        &object_key,
        &a_public,
        1024,
    )
    .unwrap();
    assert_eq!(
        opened.plaintext().as_bytes(),
        fixture.chat_plaintext.as_bytes()
    );
    // After the revocation the head epoch moved: an old-epoch record no
    // longer matches the current content binding.
    assert!(
        content::open(
            &unb64(&fixture.chat_record),
            &state.content_binding(object_id, a_id),
            ContentPurpose::ChatUpdate,
            &object_key,
            &a_public,
            1024,
        )
        .is_err()
    );

    // Enrollment proof and pairing code.
    let request = EnrollmentRequest {
        vault_id,
        request_id: unhex::<16>(&fixture.enrollment.request_id),
        device_id: unhex::<16>(&fixture.enrollment.device_id),
        signing_key: unhex::<32>(&fixture.enrollment.signing_key),
        encryption_key: unhex::<32>(&fixture.enrollment.encryption_key),
    };
    request
        .verify(&unhex::<64>(&fixture.enrollment.proof))
        .unwrap();
    assert_eq!(
        request.pairing_code(added.genesis_hash()),
        fixture.enrollment.pairing_code
    );
}
