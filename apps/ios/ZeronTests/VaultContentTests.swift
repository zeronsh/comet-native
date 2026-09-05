import CryptoKit
import XCTest
@testable import Zeron

final class VaultContentTests: XCTestCase {
    private func setup() throws -> (VaultRecordBinding, VaultContentKey, VaultDeviceSigner) {
        let binding = VaultRecordBinding(
            kind: .content, vaultId: Data(repeating: 1, count: 16), generation: Data(repeating: 2, count: 16), epoch: 1,
            objectId: Data(repeating: 3, count: 16), authorId: Data(repeating: 4, count: 16), membershipHash: Data(repeating: 5, count: 32)
        )
        let key = try VaultContentKey(scope: VaultKeyScope(binding), identifier: Data(repeating: 6, count: 16), bytes: Data(repeating: 7, count: 32))
        let signer = try VaultDeviceSigner(authorId: binding.authorId, seed: Data(repeating: 8, count: 32))
        return (binding, key, signer)
    }

    func testEncryptedContentRoundTripAndFreshSeals() throws {
        let (binding, key, signer) = try setup()
        let plaintext = Data("private transcript".utf8) + Data([0, 255])
        let first = try VaultContentCrypto.seal(binding: binding, purpose: .chatUpdate, key: key, signer: signer, plaintext: plaintext, maxPlaintextBytes: 1024)
        let retry = first.encoded
        let second = try VaultContentCrypto.seal(binding: binding, purpose: .chatUpdate, key: key, signer: signer, plaintext: plaintext, maxPlaintextBytes: 1024)
        let opened = try VaultContentCrypto.open((Data([255]) + first.encoded).dropFirst(), expected: binding, purpose: .chatUpdate, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 1024)
        XCTAssertEqual(opened.plaintext, plaintext)
        XCTAssertEqual(opened.revisionId, first.revisionId)
        XCTAssertEqual(retry, first.encoded)
        XCTAssertNotEqual(first.revisionId, second.revisionId)
        XCTAssertNotEqual(first.encoded, second.encoded)
        XCTAssertNil(first.encoded.range(of: plaintext))
        XCTAssertEqual(String(reflecting: key), "ContentKey([REDACTED])")
        XCTAssertEqual(String(reflecting: signer), "DeviceSigner([REDACTED])")
        XCTAssertEqual(String(reflecting: opened), "OpenedContent([REDACTED])")
    }

    func testEncryptedContentRejectsWrongScopePurposeAuthorAndLimits() throws {
        let (binding, key, signer) = try setup()
        let sealed = try VaultContentCrypto.seal(binding: binding, purpose: .tail, key: key, signer: signer, plaintext: Data([1]), maxPlaintextBytes: 1)
        XCTAssertThrowsError(try VaultContentCrypto.open(sealed.encoded, expected: binding, purpose: .blob, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 1)) {
            XCTAssertEqual($0 as? VaultContentError, .wrongPurpose)
        }
        var changed = binding
        changed.epoch = 2
        XCTAssertThrowsError(try VaultContentCrypto.seal(binding: changed, purpose: .tail, key: key, signer: signer, plaintext: Data(), maxPlaintextBytes: 0)) {
            XCTAssertEqual($0 as? VaultContentError, .wrongScope)
        }
        changed = binding
        changed.authorId = Data(repeating: 9, count: 16)
        XCTAssertThrowsError(try VaultContentCrypto.seal(binding: changed, purpose: .tail, key: key, signer: signer, plaintext: Data(), maxPlaintextBytes: 0)) {
            XCTAssertEqual($0 as? VaultContentError, .wrongAuthor)
        }
        let wrongKey = try VaultContentKey(scope: key.scope, identifier: key.identifier, bytes: Data(repeating: 9, count: 32))
        XCTAssertThrowsError(try VaultContentCrypto.open(sealed.encoded, expected: binding, purpose: .tail, key: wrongKey, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 1)) {
            XCTAssertEqual($0 as? VaultContentError, .crypto(.authenticationFailed))
        }
        for maximum in [-1, 0, Int.max] {
            XCTAssertThrowsError(try VaultContentCrypto.seal(binding: binding, purpose: .tail, key: key, signer: signer, plaintext: Data([1]), maxPlaintextBytes: maximum)) {
                XCTAssertEqual($0 as? VaultContentError, .sizeLimitExceeded)
            }
        }
    }

    func testEncryptedContentRequiresAEADAfterValidSignature() throws {
        let (binding, key, signer) = try setup()
        let sealed = try VaultContentCrypto.seal(binding: binding, purpose: .chatUpdate, key: key, signer: signer, plaintext: Data("private".utf8), maxPlaintextBytes: 128)
        let verified = try VaultUnverifiedRecord.parse(sealed.encoded, maxPayloadBytes: 272).verify(expected: binding, trustedPublicKey: signer.publicKey)
        let signingKey = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: 8, count: 32))
        var corrupted = verified.payload
        corrupted[corrupted.index(before: corrupted.endIndex)] ^= 1
        let input = try VaultRecordCodec.signingBytes(binding: binding, revisionId: sealed.revisionId, payload: corrupted, maxPayloadBytes: 272)
        let encoded = try VaultRecordCodec.encodeSigned(binding: binding, revisionId: sealed.revisionId, payload: corrupted, signature: signingKey.signature(for: input), maxPayloadBytes: 272)
        XCTAssertThrowsError(try VaultContentCrypto.open(encoded, expected: binding, purpose: .chatUpdate, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 128)) {
            XCTAssertEqual($0 as? VaultContentError, .crypto(.authenticationFailed))
        }
        var changed = binding
        changed.membershipHash = Data(repeating: 9, count: 32)
        let changedInput = try VaultRecordCodec.signingBytes(binding: changed, revisionId: sealed.revisionId, payload: verified.payload, maxPayloadBytes: 272)
        let changedRecord = try VaultRecordCodec.encodeSigned(binding: changed, revisionId: sealed.revisionId, payload: verified.payload, signature: signingKey.signature(for: changedInput), maxPayloadBytes: 272)
        XCTAssertThrowsError(try VaultContentCrypto.open(changedRecord, expected: changed, purpose: .chatUpdate, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 128)) {
            XCTAssertEqual($0 as? VaultContentError, .crypto(.authenticationFailed))
        }
    }

    func testContentKeyGenerationAndEmptyPayloads() throws {
        let (binding, key, signer) = try setup()
        let first = try VaultContentKey.generate(scope: key.scope)
        let second = try VaultContentKey.generate(scope: key.scope)
        XCTAssertNotEqual(first.identifier, second.identifier)
        XCTAssertNotEqual(first.exposeSecret(), second.exposeSecret())
        for raw: UInt64 in 1...8 {
            let purpose = try XCTUnwrap(VaultContentPurpose(rawValue: raw))
            let sealed = try VaultContentCrypto.seal(binding: binding, purpose: purpose, key: key, signer: signer, plaintext: Data(), maxPlaintextBytes: 0)
            XCTAssertTrue(try VaultContentCrypto.open(sealed.encoded, expected: binding, purpose: purpose, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: 0).plaintext.isEmpty)
        }
        XCTAssertThrowsError(try VaultContentKey(scope: key.scope, identifier: key.identifier, bytes: Data(repeating: 0, count: 31)))
    }
}
