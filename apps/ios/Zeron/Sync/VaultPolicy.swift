import CryptoKit
import Foundation

/// Signed membership policy records (RFC 0001 §5, §7.8) — the Swift twin of
/// `crates/crypto/src/policy.rs`. A verified `VaultMembershipState` is the
/// only source of "who may sign for this vault": every content/envelope
/// verification takes its expected binding and the author's public key from
/// here, never from the record being checked.
enum VaultPolicyError: Error, Equatable {
    case record(VaultRecordError)
    case malformed
    case unsupportedVersion
    case unsupportedOperation
    case wrongVault
    case wrongProfile
    case wrongSequence
    case wrongParent
    case wrongEpoch
    case unknownAuthor
    case revokedAuthor
    case invalidDeviceSet
    case invalidRecoveryKeys
    case tooManyDevices
    case invalidTransition
}

enum VaultDeviceStatus: UInt64 {
    case active = 0
    case revoked = 1
}

enum VaultPolicyOperation: UInt64 {
    case genesis = 1, addDevice = 2, revokeDevice = 3, rotateRecovery = 4, recoveryTransition = 5
}

struct VaultDeviceEntry: Equatable {
    var deviceId: Data
    var signingKey: Data
    var encryptionKey: Data
    var status: VaultDeviceStatus
}

struct VaultPolicyPayload: Equatable {
    var sequence: UInt64
    var parentHash: Data
    var profileHash: Data
    var epoch: UInt64
    var operation: VaultPolicyOperation
    var recoverySigningKey: Data
    var recoveryEncryptionKey: Data
    var devices: [VaultDeviceEntry]

    static let maxDevices = 64

    static func decode(_ bytes: Data) throws -> VaultPolicyPayload {
        try VaultPolicy.checked {
            var reader = VaultRecordReader(bytes)
            guard try reader.argument(major: 5) == 9 else { throw VaultPolicyError.malformed }
            guard try reader.uintField(0) == 1 else { throw VaultPolicyError.unsupportedVersion }
            let sequence = try reader.uintField(1)
            let parentHash = try reader.fixedField(2, count: 32)
            let profileHash = try reader.fixedField(3, count: 32)
            let epoch = try reader.uintField(4)
            guard let operation = VaultPolicyOperation(rawValue: try reader.uintField(5)) else {
                throw VaultPolicyError.unsupportedOperation
            }
            let recoverySigningKey = try reader.fixedField(6, count: 32)
            let recoveryEncryptionKey = try reader.fixedField(7, count: 32)
            guard try reader.argument(major: 0) == 8 else { throw VaultPolicyError.malformed }
            let count = try reader.argument(major: 4)
            guard count <= UInt64(maxDevices) else { throw VaultPolicyError.tooManyDevices }
            var devices: [VaultDeviceEntry] = []
            for _ in 0..<count {
                guard try reader.argument(major: 4) == 4 else { throw VaultPolicyError.malformed }
                let deviceId = try reader.fixedBytes(16)
                let signingKey = try reader.fixedBytes(32)
                let encryptionKey = try reader.fixedBytes(32)
                guard let status = VaultDeviceStatus(rawValue: try reader.argument(major: 0)) else {
                    throw VaultPolicyError.malformed
                }
                devices.append(VaultDeviceEntry(deviceId: deviceId, signingKey: signingKey,
                                                encryptionKey: encryptionKey, status: status))
            }
            guard reader.isAtEnd else { throw VaultPolicyError.malformed }
            return VaultPolicyPayload(
                sequence: sequence, parentHash: parentHash, profileHash: profileHash, epoch: epoch,
                operation: operation, recoverySigningKey: recoverySigningKey,
                recoveryEncryptionKey: recoveryEncryptionKey, devices: devices
            )
        }
    }

    func encode() throws -> Data {
        guard devices.count <= Self.maxDevices else { throw VaultPolicyError.tooManyDevices }
        var out = Data(capacity: 256 + devices.count * 96)
        VaultRecordCodec.argument(into: &out, major: 5, value: 9)
        VaultRecordCodec.uintField(into: &out, key: 0, value: 1)
        VaultRecordCodec.uintField(into: &out, key: 1, value: sequence)
        VaultRecordCodec.bytesField(into: &out, key: 2, value: parentHash)
        VaultRecordCodec.bytesField(into: &out, key: 3, value: profileHash)
        VaultRecordCodec.uintField(into: &out, key: 4, value: epoch)
        VaultRecordCodec.uintField(into: &out, key: 5, value: operation.rawValue)
        VaultRecordCodec.bytesField(into: &out, key: 6, value: recoverySigningKey)
        VaultRecordCodec.bytesField(into: &out, key: 7, value: recoveryEncryptionKey)
        VaultRecordCodec.argument(into: &out, major: 0, value: 8)
        VaultRecordCodec.argument(into: &out, major: 4, value: UInt64(devices.count))
        for device in devices {
            VaultRecordCodec.argument(into: &out, major: 4, value: 4)
            VaultRecordCodec.argument(into: &out, major: 2, value: 16)
            out.append(device.deviceId)
            VaultRecordCodec.argument(into: &out, major: 2, value: 32)
            out.append(device.signingKey)
            VaultRecordCodec.argument(into: &out, major: 2, value: 32)
            out.append(device.encryptionKey)
            VaultRecordCodec.argument(into: &out, major: 0, value: device.status.rawValue)
        }
        return out
    }
}

/// An enrollment request (RFC §6.2): the pending device's public identity
/// bound to the vault it wants to join, with a key-possession proof.
struct VaultEnrollmentRequest: Equatable {
    var vaultId: Data
    var requestId: Data
    var deviceId: Data
    var signingKey: Data
    var encryptionKey: Data

    private var body: Data { vaultId + requestId + deviceId + signingKey + encryptionKey }

    var proofInput: Data { VaultPolicy.enrollDomain + body }

    func verify(proof: Data) throws {
        guard VaultCrypto.passesEd25519PointEncodingPrecheck(signingKey),
              !encryptionKey.allSatisfy({ $0 == 0 }),
              deviceId != VaultPolicy.policyObjectId else {
            throw VaultPolicyError.invalidDeviceSet
        }
        try VaultCrypto.verifyEd25519(publicKey: signingKey, message: proofInput, signature: proof)
    }

    /// "NNNN-NNNN": both sides derive it from the request they hold PLUS the
    /// genesis hash of the vault they see (RFC §7.8).
    func pairingCode(genesisHash: Data) -> String {
        let digest = SHA256.hash(data: VaultPolicy.pairingDomain + body + genesisHash)
        let bytes = Array(digest.prefix(4))
        let value = (UInt32(bytes[0]) << 24 | UInt32(bytes[1]) << 16 | UInt32(bytes[2]) << 8 | UInt32(bytes[3]))
            % 100_000_000
        return String(format: "%04d-%04d", value / 10_000, value % 10_000)
    }

    var deviceEntry: VaultDeviceEntry {
        VaultDeviceEntry(deviceId: deviceId, signingKey: signingKey, encryptionKey: encryptionKey, status: .active)
    }
}

enum VaultPolicy {
    static let policyObjectId = Data(repeating: 0, count: 16)
    static let maxPolicyBytes = 64 * 1024
    fileprivate static let membershipDomain = Data("zeron/membership/v1\0".utf8)
    fileprivate static let profileDomain = Data("zeron/profile/v1\0".utf8)
    fileprivate static let recoveryIdDomain = Data("zeron/recovery-id/v1\0".utf8)
    fileprivate static let enrollDomain = Data("zeron/enroll/v1\0".utf8)
    fileprivate static let pairingDomain = Data("zeron/pairing-code/v1\0".utf8)

    static func profileHash(orgId: String, userId: String) -> Data {
        Data(SHA256.hash(data: profileDomain + Data(orgId.utf8) + Data([0]) + Data(userId.utf8)))
    }

    static func membershipHash(_ record: Data) -> Data {
        Data(SHA256.hash(data: membershipDomain + record))
    }

    static func recoveryAuthorityId(recoverySigningKey: Data) -> Data {
        Data(SHA256.hash(data: recoveryIdDomain + recoverySigningKey).prefix(16))
    }

    static func policyBinding(vaultId: Data, generation: Data, epoch: UInt64, authorId: Data, parent: Data) -> VaultRecordBinding {
        VaultRecordBinding(kind: .policy, vaultId: vaultId, generation: generation, epoch: epoch,
                           objectId: policyObjectId, authorId: authorId, membershipHash: parent)
    }

    fileprivate static func checked<Value>(_ operation: () throws -> Value) throws -> Value {
        do { return try operation() }
        catch let error as VaultPolicyError { throw error }
        catch let error as VaultRecordError { throw VaultPolicyError.record(error) }
        catch { throw VaultPolicyError.malformed }
    }

    fileprivate static func validDeviceEntries(_ devices: [VaultDeviceEntry]) -> Bool {
        guard !devices.isEmpty, devices.count <= VaultPolicyPayload.maxDevices else { return false }
        for (index, device) in devices.enumerated() {
            guard VaultCrypto.passesEd25519PointEncodingPrecheck(device.signingKey),
                  !device.encryptionKey.allSatisfy({ $0 == 0 }),
                  device.deviceId != policyObjectId else { return false }
            for other in devices[..<index] where other.deviceId == device.deviceId
                || other.signingKey == device.signingKey || other.encryptionKey == device.encryptionKey {
                return false
            }
        }
        return true
    }

    fileprivate static func validRecoveryKeys(_ payload: VaultPolicyPayload) -> Bool {
        VaultCrypto.passesEd25519PointEncodingPrecheck(payload.recoverySigningKey)
            && !payload.recoveryEncryptionKey.allSatisfy({ $0 == 0 })
            && !payload.devices.contains {
                $0.signingKey == payload.recoverySigningKey || $0.encryptionKey == payload.recoveryEncryptionKey
            }
    }
}

/// A verified membership head: the trust anchor for every other record.
struct VaultMembershipState: Equatable {
    let vaultId: Data
    let generation: Data
    let genesisHash: Data
    let hash: Data
    let sequence: UInt64
    let epoch: UInt64
    let profileHash: Data
    let recoverySigningKey: Data
    let recoveryEncryptionKey: Data
    let devices: [VaultDeviceEntry]

    var recoveryAuthorityId: Data { VaultPolicy.recoveryAuthorityId(recoverySigningKey: recoverySigningKey) }

    func device(_ id: Data) -> VaultDeviceEntry? { devices.first { $0.deviceId == id } }
    func activeDevice(_ id: Data) -> VaultDeviceEntry? {
        device(id).flatMap { $0.status == .active ? $0 : nil }
    }

    /// Pin a genesis record against LOCALLY expected identity (never the
    /// server's descriptor alone).
    static func fromGenesis(_ encoded: Data, expectedVaultId: Data, expectedGeneration: Data,
                            expectedProfileHash: Data) throws -> VaultMembershipState {
        try VaultPolicy.checked {
            let parsed = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: VaultPolicy.maxPolicyBytes)
            let untrusted = parsed.untrustedBinding
            guard untrusted.kind == .policy, untrusted.vaultId == expectedVaultId,
                  untrusted.generation == expectedGeneration, untrusted.objectId == VaultPolicy.policyObjectId else {
                throw VaultPolicyError.wrongVault
            }
            let payload = try VaultPolicyPayload.decode(try payloadOf(encoded))
            guard payload.operation == .genesis else { throw VaultPolicyError.invalidTransition }
            guard payload.sequence == 0, payload.parentHash.allSatisfy({ $0 == 0 }) else { throw VaultPolicyError.wrongSequence }
            guard payload.profileHash == expectedProfileHash else { throw VaultPolicyError.wrongProfile }
            guard payload.epoch == 1 else { throw VaultPolicyError.wrongEpoch }
            guard VaultPolicy.validDeviceEntries(payload.devices), payload.devices.count == 1,
                  let device = payload.devices.first, device.status == .active else {
                throw VaultPolicyError.invalidDeviceSet
            }
            guard VaultPolicy.validRecoveryKeys(payload) else { throw VaultPolicyError.invalidRecoveryKeys }
            let expected = VaultPolicy.policyBinding(vaultId: expectedVaultId, generation: expectedGeneration,
                                                     epoch: 1, authorId: device.deviceId,
                                                     parent: Data(repeating: 0, count: 32))
            _ = try parsed.verify(expected: expected, trustedPublicKey: device.signingKey)
            let hash = VaultPolicy.membershipHash(encoded)
            return VaultMembershipState(
                vaultId: expectedVaultId, generation: expectedGeneration, genesisHash: hash, hash: hash,
                sequence: 0, epoch: 1, profileHash: payload.profileHash,
                recoverySigningKey: payload.recoverySigningKey,
                recoveryEncryptionKey: payload.recoveryEncryptionKey, devices: payload.devices
            )
        }
    }

    /// Verify and apply the next record in the history.
    func apply(_ encoded: Data) throws -> VaultMembershipState {
        try VaultPolicy.checked {
            let parsed = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: VaultPolicy.maxPolicyBytes)
            let untrusted = parsed.untrustedBinding
            guard untrusted.kind == .policy, untrusted.vaultId == vaultId, untrusted.generation == generation,
                  untrusted.objectId == VaultPolicy.policyObjectId else { throw VaultPolicyError.wrongVault }
            guard untrusted.membershipHash == hash else { throw VaultPolicyError.wrongParent }
            let payload = try VaultPolicyPayload.decode(try Self.payloadOf(encoded))
            guard sequence < UInt64.max, payload.sequence == sequence + 1 else { throw VaultPolicyError.wrongSequence }
            guard payload.parentHash == hash else { throw VaultPolicyError.wrongParent }
            guard payload.profileHash == profileHash else { throw VaultPolicyError.wrongProfile }
            guard VaultPolicy.validDeviceEntries(payload.devices) else { throw VaultPolicyError.invalidDeviceSet }
            guard VaultPolicy.validRecoveryKeys(payload) else { throw VaultPolicyError.invalidRecoveryKeys }
            guard epoch < UInt64.max else { throw VaultPolicyError.wrongEpoch }
            let signingKey: Data
            switch payload.operation {
            case .genesis:
                throw VaultPolicyError.invalidTransition
            case .recoveryTransition:
                guard untrusted.authorId == recoveryAuthorityId else { throw VaultPolicyError.unknownAuthor }
                signingKey = recoverySigningKey
            default:
                guard let author = device(untrusted.authorId) else { throw VaultPolicyError.unknownAuthor }
                guard author.status == .active else { throw VaultPolicyError.revokedAuthor }
                signingKey = author.signingKey
            }
            let expectedEpoch = payload.operation == .addDevice ? epoch : epoch + 1
            guard payload.epoch == expectedEpoch, untrusted.epoch == expectedEpoch else { throw VaultPolicyError.wrongEpoch }
            try checkTransition(payload)
            let expected = VaultPolicy.policyBinding(vaultId: vaultId, generation: generation, epoch: expectedEpoch,
                                                     authorId: untrusted.authorId, parent: hash)
            _ = try parsed.verify(expected: expected, trustedPublicKey: signingKey)
            return VaultMembershipState(
                vaultId: vaultId, generation: generation, genesisHash: genesisHash,
                hash: VaultPolicy.membershipHash(encoded), sequence: payload.sequence, epoch: payload.epoch,
                profileHash: profileHash, recoverySigningKey: payload.recoverySigningKey,
                recoveryEncryptionKey: payload.recoveryEncryptionKey, devices: payload.devices
            )
        }
    }

    private func checkTransition(_ payload: VaultPolicyPayload) throws {
        let recoveryUnchanged = payload.recoverySigningKey == recoverySigningKey
            && payload.recoveryEncryptionKey == recoveryEncryptionKey
        let recoveryReplaced = payload.recoverySigningKey != recoverySigningKey
            && payload.recoveryEncryptionKey != recoveryEncryptionKey
        let sameDevice: (VaultDeviceEntry, VaultDeviceEntry) -> Bool = {
            $0.deviceId == $1.deviceId && $0.signingKey == $1.signingKey && $0.encryptionKey == $1.encryptionKey
        }
        func prefix(_ accept: (VaultDeviceEntry, VaultDeviceEntry) -> Bool) throws {
            guard payload.devices.count >= devices.count else { throw VaultPolicyError.invalidDeviceSet }
            for (previous, next) in zip(devices, payload.devices) where !accept(previous, next) {
                throw VaultPolicyError.invalidDeviceSet
            }
        }
        let added = Array(payload.devices.dropFirst(devices.count))
        switch payload.operation {
        case .genesis:
            throw VaultPolicyError.invalidTransition
        case .addDevice:
            guard recoveryUnchanged else { throw VaultPolicyError.invalidRecoveryKeys }
            try prefix { $0 == $1 }
            guard added.count == 1, added[0].status == .active else { throw VaultPolicyError.invalidDeviceSet }
        case .revokeDevice:
            guard recoveryUnchanged else { throw VaultPolicyError.invalidRecoveryKeys }
            guard payload.devices.count == devices.count else { throw VaultPolicyError.invalidDeviceSet }
            var revoked = 0
            try prefix { previous, next in
                if previous == next { return true }
                if sameDevice(previous, next), previous.status == .active, next.status == .revoked {
                    revoked += 1
                    return true
                }
                return false
            }
            guard revoked == 1 else { throw VaultPolicyError.invalidDeviceSet }
        case .rotateRecovery:
            guard recoveryReplaced else { throw VaultPolicyError.invalidRecoveryKeys }
            guard payload.devices == devices else { throw VaultPolicyError.invalidDeviceSet }
        case .recoveryTransition:
            guard recoveryUnchanged || recoveryReplaced else { throw VaultPolicyError.invalidRecoveryKeys }
            try prefix { previous, next in
                previous == next || (sameDevice(previous, next) && next.status == .revoked)
            }
            guard added.count == 1, added[0].status == .active else { throw VaultPolicyError.invalidDeviceSet }
        }
    }

    /// Binding for a content record written to `objectId` by `authorId`
    /// under the current epoch and this membership.
    func contentBinding(objectId: Data, authorId: Data) -> VaultRecordBinding {
        VaultRecordBinding(kind: .content, vaultId: vaultId, generation: generation, epoch: epoch,
                           objectId: objectId, authorId: authorId, membershipHash: hash)
    }

    func envelopeBinding(objectId: Data, epoch: UInt64, authorId: Data) -> VaultRecordBinding {
        VaultRecordBinding(kind: .keyEnvelope, vaultId: vaultId, generation: generation, epoch: epoch,
                           objectId: objectId, authorId: authorId, membershipHash: hash)
    }

    private static func payloadOf(_ encoded: Data) throws -> Data {
        try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: VaultPolicy.maxPolicyBytes).untrustedPayload
    }
}

extension VaultRecordReader {
    /// A bare byte string of exactly `count` bytes (array elements carry no key).
    mutating func fixedBytes(_ count: Int) throws -> Data {
        let length = try argument(major: 2)
        guard length == UInt64(count) else { throw VaultRecordError.malformed }
        return try takeBytes(count)
    }
}
