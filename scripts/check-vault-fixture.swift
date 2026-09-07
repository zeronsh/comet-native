// Native macOS runner for the shared vault fixture: the same assertions as
// ZeronTests/VaultControlPlaneTests.swift, runnable without an iOS simulator.
//   swiftc apps/ios/Zeron/Sync/Vault*.swift scripts/check-vault-fixture.swift -o target/vault-fixture-probe
//   ./target/vault-fixture-probe crates/crypto/tests/fixtures/vault.json
import CryptoKit
import Foundation

struct Device: Decodable { let id, signingSeed, signingKey, encryptionSecret, encryptionKey: String }
struct Enrollment: Decodable { let requestId, deviceId, signingKey, encryptionKey, proof, pairingCode: String }
struct Fixture: Decodable {
    let vaultId, generation, orgId, userId, profileHash: String
    let recoverySecret, recoveryKit, recoverySigningKey, recoveryEncryptionKey, recoveryAuthorityId: String
    let deviceA, deviceB: Device
    let membership, membershipHashes: [String]
    let epochsAfter: [UInt64]
    let keyringEnvelopeB, keyringEpoch1, objectId, objectKeyEnvelope, objectKeyId, objectKey: String
    let chatRecord, chatPlaintext: String
    let enrollment: Enrollment
}

struct Failure: Error { let check: String }
var passed = 0
func check(_ name: String, _ condition: @autoclosure () throws -> Bool) throws {
    guard try condition() else { throw Failure(check: name) }
    passed += 1
    print("PASS: \(name)")
}
func throws_(_ name: String, _ operation: () throws -> Void) throws {
    do { try operation() } catch { passed += 1; print("PASS: \(name) (rejected: \(error))"); return }
    throw Failure(check: "\(name) — accepted")
}
func hex(_ text: String) -> Data {
    var out = Data(); var i = text.startIndex
    while i < text.endIndex { let n = text.index(i, offsetBy: 2); out.append(UInt8(text[i..<n], radix: 16)!); i = n }
    return out
}
func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }
func b64(_ text: String) -> Data { Data(base64Encoded: text)! }

@main
struct Probe {
    static func main() throws {
        let path = CommandLine.arguments.dropFirst().first ?? "crates/crypto/tests/fixtures/vault.json"
        let fixture = try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        let records = fixture.membership.map(b64)
        let profile = VaultPolicy.profileHash(orgId: fixture.orgId, userId: fixture.userId)
        try check("profile hash agrees", hex(profile) == fixture.profileHash)
        var states = [try VaultMembershipState.fromGenesis(records[0], expectedVaultId: hex(fixture.vaultId),
                                                            expectedGeneration: hex(fixture.generation), expectedProfileHash: profile)]
        for record in records.dropFirst() { states.append(try states.last!.apply(record)) }
        try check("membership hashes agree", states.map { hex($0.hash) } == fixture.membershipHashes)
        try check("epochs agree", states.map(\.epoch) == fixture.epochsAfter)
        try check("device B revoked at head", states.last!.device(hex(fixture.deviceB.id))?.status == .revoked)
        try check("recovery authority id agrees", hex(states.last!.recoveryAuthorityId) == fixture.recoveryAuthorityId)
        try throws_("replay on moved head") { _ = try states[1].apply(records[1]) }
        try throws_("fork onto genesis") { _ = try states[0].apply(records[2]) }
        for index in [0, 5, 40, records[1].count - 1] {
            var damaged = records[1]; damaged[damaged.startIndex + index] ^= 1
            try throws_("mutation at byte \(index)") { _ = try states[0].apply(damaged) }
        }
        try throws_("wrong profile at genesis") {
            _ = try VaultMembershipState.fromGenesis(records[0], expectedVaultId: hex(fixture.vaultId),
                                                     expectedGeneration: hex(fixture.generation), expectedProfileHash: Data(repeating: 0, count: 32))
        }

        let added = states[1]
        let aId = hex(fixture.deviceA.id), aPublic = hex(fixture.deviceA.signingKey)
        let bKey = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: hex(fixture.deviceB.encryptionSecret))
        let keyring = try VaultEnvelope.openKeyring(
            b64(fixture.keyringEnvelopeB), expected: added.envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
            recipientKind: .device, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic)
        try check("keyring envelope opens (HPKE)", hex(keyring.epochKey(1)!) == fixture.keyringEpoch1)
        try check("keyring codec round trips", try VaultKeyring.decode(keyring.encode()) == keyring)
        try throws_("keyring envelope wrong recipient kind") {
            _ = try VaultEnvelope.openKeyring(b64(fixture.keyringEnvelopeB), expected: added.envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
                recipientKind: .recovery, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic)
        }
        try throws_("keyring envelope stale head") {
            _ = try VaultEnvelope.openKeyring(b64(fixture.keyringEnvelopeB), expected: states[2].envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: 1, authorId: aId),
                recipientKind: .device, recipientId: hex(fixture.deviceB.id), recipientKey: bKey, trustedPublicKey: aPublic)
        }
        let objectId = hex(fixture.objectId)
        let objectKey = try VaultEnvelope.unwrapObjectKey(b64(fixture.objectKeyEnvelope),
            expected: added.envelopeBinding(objectId: objectId, epoch: 1, authorId: aId), epochKey: keyring.epochKey(1)!, trustedPublicKey: aPublic)
        try check("object key unwraps", hex(objectKey.identifier) == fixture.objectKeyId && hex(objectKey.exposeSecret()) == fixture.objectKey)
        let opened = try VaultContentCrypto.open(b64(fixture.chatRecord), expected: added.contentBinding(objectId: objectId, authorId: aId),
            purpose: .chatUpdate, key: objectKey, trustedPublicKey: aPublic, maxPlaintextBytes: 1024)
        try check("chat record opens", String(decoding: opened.plaintext, as: UTF8.self) == fixture.chatPlaintext)
        try throws_("chat record under moved head") {
            _ = try VaultContentCrypto.open(b64(fixture.chatRecord), expected: states[2].contentBinding(objectId: objectId, authorId: aId),
                purpose: .chatUpdate, key: objectKey, trustedPublicKey: aPublic, maxPlaintextBytes: 1024)
        }

        let secret = try VaultRecoverySecret(kit: fixture.recoveryKit)
        try check("recovery kit decodes", hex(secret.secret) == fixture.recoverySecret && secret.kit == fixture.recoveryKit)
        try check("recovery signing key agrees", hex(try secret.signingKey().publicKey.rawRepresentation) == fixture.recoverySigningKey)
        try check("recovery encryption key agrees", hex(try secret.encryptionKey().publicKey.rawRepresentation) == fixture.recoveryEncryptionKey)
        try check("recovery authority id from kit", hex(try secret.authorityId()) == fixture.recoveryAuthorityId)
        try check("kit parsing is tolerant", try VaultRecoverySecret(kit: fixture.recoveryKit.lowercased().replacingOccurrences(of: "-", with: " ")).secret == secret.secret)
        var damagedKit = Array(fixture.recoveryKit); damagedKit[0] = damagedKit[0] == "A" ? "B" : "A"
        try throws_("kit checksum") { _ = try VaultRecoverySecret(kit: String(damagedKit)) }

        let request = VaultEnrollmentRequest(vaultId: hex(fixture.vaultId), requestId: hex(fixture.enrollment.requestId),
            deviceId: hex(fixture.enrollment.deviceId), signingKey: hex(fixture.enrollment.signingKey), encryptionKey: hex(fixture.enrollment.encryptionKey))
        try request.verify(proof: hex(fixture.enrollment.proof)); passed += 1; print("PASS: enrollment proof verifies")
        let genesis = hex(fixture.membershipHashes[0])
        try check("pairing code agrees", request.pairingCode(genesisHash: genesis) == fixture.enrollment.pairingCode)
        var swapped = request; swapped.encryptionKey = Data(repeating: 9, count: 32)
        try throws_("enrollment proof with swapped key") { try swapped.verify(proof: hex(fixture.enrollment.proof)) }
        try check("pairing code differs for another vault", request.pairingCode(genesisHash: Data(repeating: 0, count: 32)) != fixture.enrollment.pairingCode)
        print("\(passed) checks passed")
    }
}
