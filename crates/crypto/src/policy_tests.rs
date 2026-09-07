use super::*;
use crate::content::DeviceSigner;
use crate::hpke::HpkePrivateKey;
use crate::recovery::RecoverySecret;

struct Device {
    id: [u8; 16],
    signer: DeviceSigner,
    encryption: HpkePrivateKey,
}

impl Device {
    fn new(tag: u8) -> Self {
        let mut seed = [tag; 32];
        seed[0] ^= 0x5a;
        Self {
            id: [tag; 16],
            signer: DeviceSigner::from_seed([tag; 16], &seed).unwrap(),
            encryption: HpkePrivateKey::from_bytes(&[tag ^ 0x33; 32]).unwrap(),
        }
    }

    fn entry(&self, status: DeviceStatus) -> DeviceEntry {
        DeviceEntry {
            device_id: self.id,
            signing_key: self.signer.public_key().try_into().unwrap(),
            encryption_key: *self.encryption.public_key().as_bytes(),
            status,
        }
    }
}

const VAULT: [u8; 16] = [1; 16];
const GENERATION: [u8; 16] = [2; 16];

fn recovery() -> RecoverySecret {
    RecoverySecret::from_bytes(&[77; 32]).unwrap()
}

fn genesis_payload(device: &Device, recovery: &RecoverySecret) -> PolicyPayload {
    PolicyPayload {
        sequence: 0,
        parent_hash: [0; 32],
        profile_hash: profile_hash("org", "user"),
        epoch: 1,
        operation: Operation::Genesis,
        recovery_signing_key: recovery.signing_public_key().unwrap(),
        recovery_encryption_key: *recovery.encryption_key().unwrap().public_key().as_bytes(),
        devices: vec![device.entry(DeviceStatus::Active)],
    }
}

fn genesis(device: &Device) -> (Vec<u8>, MembershipState) {
    let recovery = recovery();
    let payload = genesis_payload(device, &recovery);
    let binding = policy_binding(VAULT, GENERATION, 1, device.id, [0; 32]);
    let encoded = encode_policy(&binding, &payload, &device.signer).unwrap();
    let state =
        MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile_hash("org", "user"))
            .unwrap();
    (encoded, state)
}

fn sign_next(
    state: &MembershipState,
    payload: &PolicyPayload,
    author: [u8; 16],
    signer: &DeviceSigner,
) -> Vec<u8> {
    let binding = policy_binding(VAULT, GENERATION, payload.epoch, author, *state.hash());
    encode_policy(&binding, payload, signer).unwrap()
}

#[test]
fn genesis_pins_the_initial_device_and_rejects_wrong_expectations() {
    let device = Device::new(10);
    let (encoded, state) = genesis(&device);
    assert_eq!(state.sequence(), 0);
    assert_eq!(state.epoch(), 1);
    assert_eq!(state.hash(), &membership_hash(&encoded));
    assert_eq!(state.genesis_hash(), state.hash());
    assert!(state.active_device(&device.id).is_some());
    assert_eq!(
        state.recovery_authority_id(),
        recovery_authority_id(&recovery().signing_public_key().unwrap())
    );
    let profile = profile_hash("org", "user");
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &[9; 16], &GENERATION, &profile),
        Err(PolicyError::WrongVault)
    ));
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &VAULT, &[9; 16], &profile),
        Err(PolicyError::WrongVault)
    ));
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile_hash("org", "x")),
        Err(PolicyError::WrongProfile)
    ));
    // Every single-byte mutation of the record fails closed.
    for index in 0..encoded.len() {
        let mut damaged = encoded.clone();
        damaged[index] ^= 0x01;
        assert!(
            MembershipState::from_genesis(&damaged, &VAULT, &GENERATION, &profile).is_err(),
            "byte {index} mutation accepted"
        );
    }
    // A genesis cannot be applied on top of itself.
    assert!(matches!(
        state.apply(&encoded),
        Err(PolicyError::WrongParent) | Err(PolicyError::WrongSequence)
    ));
}

#[test]
fn genesis_requires_a_self_signed_single_active_device_at_epoch_one() {
    let device = Device::new(10);
    let other = Device::new(11);
    let recovery = recovery();
    let profile = profile_hash("org", "user");
    // Signed by a device that is not the listed member.
    let payload = genesis_payload(&device, &recovery);
    let binding = policy_binding(VAULT, GENERATION, 1, other.id, [0; 32]);
    let encoded = encode_policy(&binding, &payload, &other.signer).unwrap();
    assert!(MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile).is_err());
    // Two devices at genesis.
    let mut payload = genesis_payload(&device, &recovery);
    payload.devices.push(other.entry(DeviceStatus::Active));
    let binding = policy_binding(VAULT, GENERATION, 1, device.id, [0; 32]);
    let encoded = encode_policy(&binding, &payload, &device.signer).unwrap();
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile),
        Err(PolicyError::InvalidDeviceSet)
    ));
    // Wrong epoch.
    let mut payload = genesis_payload(&device, &recovery);
    payload.epoch = 2;
    let binding = policy_binding(VAULT, GENERATION, 2, device.id, [0; 32]);
    let encoded = encode_policy(&binding, &payload, &device.signer).unwrap();
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile),
        Err(PolicyError::WrongEpoch)
    ));
    // Recovery keys may not coincide with a device's keys.
    let mut payload = genesis_payload(&device, &recovery);
    payload.recovery_signing_key = payload.devices[0].signing_key;
    let binding = policy_binding(VAULT, GENERATION, 1, device.id, [0; 32]);
    let encoded = encode_policy(&binding, &payload, &device.signer).unwrap();
    assert!(matches!(
        MembershipState::from_genesis(&encoded, &VAULT, &GENERATION, &profile),
        Err(PolicyError::InvalidRecoveryKeys)
    ));
}

#[test]
fn add_and_revoke_devices_follow_the_transition_rules() {
    let first = Device::new(10);
    let second = Device::new(11);
    let (_, state) = genesis(&first);

    let mut add = state.next_payload(Operation::AddDevice);
    add.devices.push(second.entry(DeviceStatus::Active));
    assert_eq!(add.epoch, 1, "adding a device keeps the write epoch");
    let encoded = sign_next(&state, &add, first.id, &first.signer);
    let state = state.apply(&encoded).unwrap();
    assert_eq!(state.sequence(), 1);
    assert_eq!(state.devices().len(), 2);
    assert!(state.active_device(&second.id).is_some());

    // The new device may now authorize changes; an unknown one may not.
    let third = Device::new(12);
    let mut add_third = state.next_payload(Operation::AddDevice);
    add_third.devices.push(third.entry(DeviceStatus::Active));
    let by_second = sign_next(&state, &add_third, second.id, &second.signer);
    let state_with_third = state.apply(&by_second).unwrap();
    let by_third = sign_next(&state, &add_third, third.id, &third.signer);
    assert!(matches!(
        state.apply(&by_third),
        Err(PolicyError::UnknownAuthor)
    ));

    // Revocation must bump the epoch and flip exactly one device.
    let mut revoke = state_with_third.next_payload(Operation::RevokeDevice);
    revoke.devices[2].status = DeviceStatus::Revoked;
    assert_eq!(revoke.epoch, 2);
    let encoded = sign_next(&state_with_third, &revoke, first.id, &first.signer);
    let revoked = state_with_third.apply(&encoded).unwrap();
    assert_eq!(revoked.epoch(), 2);
    assert!(revoked.active_device(&third.id).is_none());
    assert!(
        revoked.device(&third.id).is_some(),
        "history keeps the entry"
    );

    // The revoked device cannot sign further changes.
    let mut readd = revoked.next_payload(Operation::AddDevice);
    readd
        .devices
        .push(Device::new(13).entry(DeviceStatus::Active));
    let encoded = sign_next(&revoked, &readd, third.id, &third.signer);
    assert!(matches!(
        revoked.apply(&encoded),
        Err(PolicyError::RevokedAuthor)
    ));
    // Nor can anyone revive it.
    let mut revive = revoked.next_payload(Operation::AddDevice);
    revive.devices[2].status = DeviceStatus::Active;
    let encoded = sign_next(&revoked, &revive, first.id, &first.signer);
    assert!(matches!(
        revoked.apply(&encoded),
        Err(PolicyError::InvalidDeviceSet)
    ));
    // A revoke that forgets the epoch bump is rejected.
    let mut lazy = revoked.next_payload(Operation::RevokeDevice);
    lazy.devices[1].status = DeviceStatus::Revoked;
    lazy.epoch = revoked.epoch();
    let encoded = sign_next(&revoked, &lazy, first.id, &first.signer);
    assert!(matches!(
        revoked.apply(&encoded),
        Err(PolicyError::WrongEpoch)
    ));
    // An add that also touches the recovery keys is rejected.
    let mut sneaky = revoked.next_payload(Operation::AddDevice);
    sneaky
        .devices
        .push(Device::new(14).entry(DeviceStatus::Active));
    sneaky.recovery_signing_key = Device::new(15).entry(DeviceStatus::Active).signing_key;
    let encoded = sign_next(&revoked, &sneaky, first.id, &first.signer);
    assert!(matches!(
        revoked.apply(&encoded),
        Err(PolicyError::InvalidRecoveryKeys)
    ));
}

#[test]
fn history_links_reject_forks_replays_and_stale_parents() {
    let first = Device::new(10);
    let second = Device::new(11);
    let (_, state) = genesis(&first);
    let mut add = state.next_payload(Operation::AddDevice);
    add.devices.push(second.entry(DeviceStatus::Active));
    let encoded = sign_next(&state, &add, first.id, &first.signer);
    let next = state.apply(&encoded).unwrap();
    // Replaying the same record on the new head fails (parent moved).
    assert!(matches!(
        next.apply(&encoded),
        Err(PolicyError::WrongParent)
    ));
    // A concurrent record built on the OLD head is rejected by the new head.
    let mut fork = state.next_payload(Operation::AddDevice);
    fork.devices
        .push(Device::new(12).entry(DeviceStatus::Active));
    let fork_encoded = sign_next(&state, &fork, first.id, &first.signer);
    assert!(matches!(
        next.apply(&fork_encoded),
        Err(PolicyError::WrongParent)
    ));
    // A record whose payload claims the right parent but whose wrapper
    // membership hash is stale is rejected before signature checks.
    let mut skip = next.next_payload(Operation::AddDevice);
    skip.devices
        .push(Device::new(12).entry(DeviceStatus::Active));
    skip.sequence += 1;
    let encoded = sign_next(&next, &skip, first.id, &first.signer);
    assert!(matches!(
        next.apply(&encoded),
        Err(PolicyError::WrongSequence)
    ));
    // Wrong vault in the wrapper.
    let good = {
        let mut payload = next.next_payload(Operation::AddDevice);
        payload
            .devices
            .push(Device::new(12).entry(DeviceStatus::Active));
        payload
    };
    let binding = policy_binding([9; 16], GENERATION, 1, first.id, *next.hash());
    let encoded = encode_policy(&binding, &good, &first.signer).unwrap();
    assert!(matches!(next.apply(&encoded), Err(PolicyError::WrongVault)));
}

#[test]
fn recovery_authority_can_transition_and_rotate() {
    let first = Device::new(10);
    let (_, state) = genesis(&first);
    let recovery = recovery();
    let replacement = Device::new(20);

    // Recovery transition: authored by the recovery authority, revokes the
    // lost device and enrolls the replacement under a fresh epoch.
    let mut transition = state.next_payload(Operation::RecoveryTransition);
    transition.devices[0].status = DeviceStatus::Revoked;
    transition
        .devices
        .push(replacement.entry(DeviceStatus::Active));
    assert_eq!(transition.epoch, 2);
    let signer = recovery.signer().unwrap();
    let encoded = sign_next(&state, &transition, *signer.author_id(), &signer);
    let recovered = state.apply(&encoded).unwrap();
    assert_eq!(recovered.epoch(), 2);
    assert!(recovered.active_device(&replacement.id).is_some());
    assert!(recovered.active_device(&first.id).is_none());

    // A device cannot author a recovery transition.
    let encoded = sign_next(&state, &transition, first.id, &first.signer);
    assert!(matches!(
        state.apply(&encoded),
        Err(PolicyError::UnknownAuthor)
    ));
    // The wrong recovery secret cannot either.
    let wrong = RecoverySecret::from_bytes(&[78; 32])
        .unwrap()
        .signer()
        .unwrap();
    let encoded = sign_next(&state, &transition, *wrong.author_id(), &wrong);
    assert!(matches!(
        state.apply(&encoded),
        Err(PolicyError::UnknownAuthor)
    ));

    // Rotate the recovery authority from a trusted device.
    let new_recovery = RecoverySecret::from_bytes(&[79; 32]).unwrap();
    let mut rotate = recovered.next_payload(Operation::RotateRecovery);
    rotate.recovery_signing_key = new_recovery.signing_public_key().unwrap();
    rotate.recovery_encryption_key = *new_recovery
        .encryption_key()
        .unwrap()
        .public_key()
        .as_bytes();
    let encoded = sign_next(&recovered, &rotate, replacement.id, &replacement.signer);
    let rotated = recovered.apply(&encoded).unwrap();
    assert_eq!(rotated.epoch(), 3);
    assert_eq!(
        rotated.recovery_authority_id(),
        *new_recovery.signer().unwrap().author_id()
    );
    // The old authority is no longer honored.
    let mut late = rotated.next_payload(Operation::RecoveryTransition);
    late.devices
        .push(Device::new(21).entry(DeviceStatus::Active));
    let encoded = sign_next(&rotated, &late, *signer.author_id(), &signer);
    assert!(matches!(
        rotated.apply(&encoded),
        Err(PolicyError::UnknownAuthor)
    ));
    // Half-rotating the recovery keys is not a rotation.
    let mut half = rotated.next_payload(Operation::RotateRecovery);
    half.recovery_signing_key = recovery.signing_public_key().unwrap();
    let encoded = sign_next(&rotated, &half, replacement.id, &replacement.signer);
    assert!(matches!(
        rotated.apply(&encoded),
        Err(PolicyError::InvalidRecoveryKeys)
    ));
}

#[test]
fn payload_codec_round_trips_and_rejects_malformed() {
    let device = Device::new(10);
    let payload = genesis_payload(&device, &recovery());
    let encoded = payload.encode().unwrap();
    assert_eq!(PolicyPayload::decode(&encoded).unwrap(), payload);
    assert!(PolicyPayload::decode(&encoded[..encoded.len() - 1]).is_err());
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(PolicyPayload::decode(&trailing).is_err());
    let mut version = encoded.clone();
    version[2] = 2;
    assert!(matches!(
        PolicyPayload::decode(&version),
        Err(PolicyError::UnsupportedVersion)
    ));
    let mut too_many = payload.clone();
    too_many.devices = (0..MAX_DEVICES as u8 + 1)
        .map(|tag| Device::new(tag.wrapping_add(30)).entry(DeviceStatus::Active))
        .collect();
    assert!(matches!(
        too_many.encode(),
        Err(PolicyError::TooManyDevices)
    ));
    assert_ne!(profile_hash("a", "b"), profile_hash("ab", ""));
}

#[test]
fn enrollment_proofs_bind_keys_and_pairing_codes_agree() {
    let device = Device::new(40);
    let request = EnrollmentRequest {
        vault_id: VAULT,
        request_id: [3; 16],
        device_id: device.id,
        signing_key: device.signer.public_key().try_into().unwrap(),
        encryption_key: *device.encryption.public_key().as_bytes(),
    };
    let proof = request.sign(&device.signer).unwrap();
    request.verify(&proof).unwrap();
    let mut substituted = request;
    substituted.encryption_key = [9; 32];
    assert!(substituted.verify(&proof).is_err());
    assert_ne!(
        substituted.pairing_code(&[1; 32]),
        request.pairing_code(&[1; 32])
    );
    assert_ne!(
        request.pairing_code(&[2; 32]),
        request.pairing_code(&[1; 32])
    );
    let code = request.pairing_code(&[1; 32]);
    assert_eq!(code.len(), 9);
    assert!(code[..4].bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(&code[4..5], "-");
    let other = Device::new(41);
    assert!(matches!(
        request.sign(&other.signer),
        Err(PolicyError::UnknownAuthor)
    ));
}
