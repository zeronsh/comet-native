import CryptoKit
import XCTest
@testable import Zeron

/// Cross-language conformance against the Rust-generated fixture
/// (`crates/crypto/tests/fixtures/vault.json`): the membership chain, hashes,
/// epochs, keyring/object-key envelopes, the sealed chat record, the
/// recovery kit, and the enrollment proof / pairing code must all agree.
final class VaultControlPlaneTests: XCTestCase {
    private struct Fixture: Decodable {
        struct Device: Decodable { let id, signingSeed, signingKey, encryptionSecret, encryptionKey: String }
        struct Enrollment: Decodable { let requestId, deviceId, signingKey, encryptionKey, proof, pairingCode: String }
        let vaultId, generation, orgId, userId, profileHash: String
        let recoverySecret, recoveryKit, recoverySigningKey, recoveryEncryptionKey, recoveryAuthorityId: String
        let deviceA, deviceB: Device
        let membership, membershipHashes: [String]
        let epochsAfter: [UInt64]
        let keyringEnvelopeB, keyringEpoch1, objectId, objectKeyEnvelope, objectKeyId, objectKey: String
        let chatRecord, chatPlaintext: String
        let enrollment: Enrollment
    }

    private func loadFixture() throws -> Fixture {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("crates/crypto/tests/fixtures/vault.json")
        return try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: url))
    }

    private func hex(_ text: String) -> Data {
        var out = Data(capacity: text.count / 2)
        var index = text.startIndex
        while index < text.endIndex {
            let next = text.index(index, offsetBy: 2)
            out.append(UInt8(text[index..<next], radix: 16)!)
            index = next
        }
        return out
    }

    private func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }
    private func b64(_ text: String) -> Data { Data(base64Encoded: text)! }

    private func chain(_ fixture: Fixture) throws -> [VaultMembershipState] {
        let records = fixture.membership.map(b64)
        var states = [try VaultMembershipState.fromGenesis(
            records[0], expectedVaultId: hex(fixture.vaultId), expectedGeneration: hex(fixture.generation),
            expectedProfileHash: VaultPolicy.profileHash(orgId: fixture.orgId, userId: fixture.userId)
        )]
        for record in records.dropFirst() { states.append(try states.last!.apply(record)) }
        return states
    }

    func testMembershipChainAgreesWithRust() throws {
        let fixture = try loadFixture()
        XCTAssertEqual(hex(VaultPolicy.profileHash(orgId: fixture.orgId, userId: fixture.userId)), fixture.profileHash)
        let states = try chain(fixture)
        XCTAssertEqual(states.map { hex($0.hash) }, fixture.membershipHashes)
        XCTAssertEqual(states.map(\.epoch), fixture.epochsAfter)
        let head = states.last!
        XCTAssertNotNil(head.activeDevice(hex(fixture.deviceA.id)))
        XCTAssertNil(head.activeDevice(hex(fixture.deviceB.id)))
        XCTAssertEqual(head.device(hex(fixture.deviceB.id))?.status, .revoked)
        XCTAssertEqual(hex(head.recoveryAuthorityId), fixture.recoveryAuthorityId)
        // Replays, forks, and mutations fail closed.
        let records = fixture.membership.map(b64)
        XCTAssertThrowsError(try states[1].apply(records[1]))
        XCTAssertThrowsError(try states[0].apply(records[2]))
        for index in [0, 5, 40, records[1].count - 1] {
            var damaged = records[1]
            damaged[damaged.startIndex + index] ^= 1
            XCTAssertThrowsError(try states[0].apply(damaged), "byte \(index)")
        }
        XCTAssertThrowsError(try VaultMembershipState.fromGenesis(
            records[0], expectedVaultId: hex(fixture.vaultId), expectedGeneration: hex(fixture.generation),
            expectedProfileHash: Data(repeating: 0, count: 32)
        )) { XCTAssertEqual($0 as? VaultPolicyError, .wrongProfile) }
    }

    func testKeyringAndObjectKeyEnvelopesOpenTheChatRecord() throws {
        let fixture = try loadFixture()
        let states = try chain(fixture)
        let added = states[1]
        let aId = hex(fixture.deviceA.id)
        let aPublic = hex(fixture.deviceA.signingKey)
        let bKey = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: hex(fixture.deviceB.encryptionSecret))
        let keyring = try VaultEnvelope.openKeyring(
            b64(fixture.keyringEnvelopeB),
            expected: added.envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
            recipientKind: .device, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic
        )
        XCTAssertEqual(hex(keyring.epochKey(1)!), fixture.keyringEpoch1)
        XCTAssertEqual(try VaultKeyring.decode(keyring.encode()), keyring)
        // The wrong recipient or a stale head cannot open it.
        XCTAssertThrowsError(try VaultEnvelope.openKeyring(
            b64(fixture.keyringEnvelopeB),
            expected: added.envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
            recipientKind: .recovery, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic
        ))
        XCTAssertThrowsError(try VaultEnvelope.openKeyring(
            b64(fixture.keyringEnvelopeB),
            expected: states[2].envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
            recipientKind: .device, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic
        ))
        let objectId = hex(fixture.objectId)
        let objectKey = try VaultEnvelope.unwrapObjectKey(
            b64(fixture.objectKeyEnvelope),
            expected: added.envelopeBinding(objectId: objectId, epoch: 1, authorId: aId),
            epochKey: keyring.epochKey(1)!, trustedPublicKey: aPublic
        )
        XCTAssertEqual(hex(objectKey.identifier), fixture.objectKeyId)
        XCTAssertEqual(hex(objectKey.exposeSecret()), fixture.objectKey)
        let opened = try VaultContentCrypto.open(
            b64(fixture.chatRecord), expected: added.contentBinding(objectId: objectId, authorId: aId),
            purpose: .chatUpdate, key: objectKey, trustedPublicKey: aPublic, maxPlaintextBytes: 1024
        )
        XCTAssertEqual(String(decoding: opened.plaintext, as: UTF8.self), fixture.chatPlaintext)
        // After the revocation the head epoch moved on: the old record no
        // longer matches the current binding.
        XCTAssertThrowsError(try VaultContentCrypto.open(
            b64(fixture.chatRecord), expected: states[2].contentBinding(objectId: objectId, authorId: aId),
            purpose: .chatUpdate, key: objectKey, trustedPublicKey: aPublic, maxPlaintextBytes: 1024
        ))
    }

    func testRecoveryKitAndEnrollmentAgreeWithRust() throws {
        let fixture = try loadFixture()
        let secret = try VaultRecoverySecret(kit: fixture.recoveryKit)
        XCTAssertEqual(hex(secret.secret), fixture.recoverySecret)
        XCTAssertEqual(secret.kit, fixture.recoveryKit)
        XCTAssertEqual(hex(try secret.signingKey().publicKey.rawRepresentation), fixture.recoverySigningKey)
        XCTAssertEqual(hex(try secret.encryptionKey().publicKey.rawRepresentation), fixture.recoveryEncryptionKey)
        XCTAssertEqual(hex(try secret.authorityId()), fixture.recoveryAuthorityId)
        XCTAssertEqual(try VaultRecoverySecret(kit: fixture.recoveryKit.lowercased().replacingOccurrences(of: "-", with: " ")).secret, secret.secret)
        var damaged = Array(fixture.recoveryKit)
        damaged[0] = damaged[0] == "A" ? "B" : "A"
        XCTAssertThrowsError(try VaultRecoverySecret(kit: String(damaged))) {
            XCTAssertEqual($0 as? VaultRecoveryError, .checksumMismatch)
        }
        let request = VaultEnrollmentRequest(
            vaultId: hex(fixture.vaultId), requestId: hex(fixture.enrollment.requestId),
            deviceId: hex(fixture.enrollment.deviceId), signingKey: hex(fixture.enrollment.signingKey),
            encryptionKey: hex(fixture.enrollment.encryptionKey)
        )
        XCTAssertNoThrow(try request.verify(proof: hex(fixture.enrollment.proof)))
        let genesis = hex(fixture.membershipHashes[0])
        XCTAssertEqual(request.pairingCode(genesisHash: genesis), fixture.enrollment.pairingCode)
        var swapped = request
        swapped.encryptionKey = Data(repeating: 9, count: 32)
        XCTAssertThrowsError(try swapped.verify(proof: hex(fixture.enrollment.proof)))
        XCTAssertNotEqual(swapped.pairingCode(genesisHash: genesis), fixture.enrollment.pairingCode)
        XCTAssertNotEqual(request.pairingCode(genesisHash: Data(repeating: 0, count: 32)), fixture.enrollment.pairingCode)
    }
}
