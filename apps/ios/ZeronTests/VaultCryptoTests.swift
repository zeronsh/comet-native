import CryptoKit
import XCTest
@testable import Zeron

final class VaultCryptoTests: XCTestCase {
    private func signedRecordSample() throws -> (Data, Data, VaultRecordBinding) {
        let binding = VaultRecordBinding(
            kind: .content, vaultId: Data(repeating: 1, count: 16), generation: Data(repeating: 2, count: 16), epoch: 24,
            objectId: Data(repeating: 3, count: 16), authorId: Data(repeating: 4, count: 16), membershipHash: Data(repeating: 6, count: 32)
        )
        let revision = Data(repeating: 5, count: 16)
        let payload = Data([0, 255, 16, 32])
        let key = Curve25519.Signing.PrivateKey()
        let input = try VaultRecordCodec.signingBytes(binding: binding, revisionId: revision, payload: payload, maxPayloadBytes: 4)
        let encoded = try VaultRecordCodec.encodeSigned(
            binding: binding, revisionId: revision, payload: payload, signature: key.signature(for: input), maxPayloadBytes: 4
        )
        return (encoded, key.publicKey.rawRepresentation, binding)
    }

    func testSignedRecordVerificationAndEveryContextField() throws {
        let (encoded, key, binding) = try signedRecordSample()
        let record = try VaultUnverifiedRecord.parse((Data([255]) + encoded).dropFirst(), maxPayloadBytes: 4)
        XCTAssertEqual(record.untrustedBinding, binding)
        XCTAssertEqual(String(reflecting: record), "UnverifiedRecord([REDACTED])")
        let verified = try record.verify(expected: binding, trustedPublicKey: key)
        XCTAssertEqual(verified.payload, Data([0, 255, 16, 32]))
        XCTAssertEqual(verified.binding, binding)
        XCTAssertEqual(verified.revisionId, Data(repeating: 5, count: 16))
        XCTAssertEqual(String(reflecting: verified), "VerifiedRecord([REDACTED])")
        var wrong = Array(repeating: binding, count: 7)
        wrong[0].kind = .policy
        wrong[1].vaultId = Data(repeating: 9, count: 16)
        wrong[2].generation = Data(repeating: 9, count: 16)
        wrong[3].epoch += 1
        wrong[4].objectId = Data(repeating: 9, count: 16)
        wrong[5].authorId = Data(repeating: 9, count: 16)
        wrong[6].membershipHash = Data(repeating: 9, count: 32)
        for context in wrong {
            XCTAssertThrowsError(try record.verify(expected: context, trustedPublicKey: key)) {
                XCTAssertEqual($0 as? VaultRecordError, .contextMismatch)
            }
        }
        for count in [31, 32] {
            XCTAssertThrowsError(try record.verify(expected: binding, trustedPublicKey: Data(repeating: 0, count: count))) {
                XCTAssertEqual($0 as? VaultRecordError, .invalidSignature)
            }
        }
    }

    func testSignedRecordRejectsEveryTruncationAndByteMutation() throws {
        let (encoded, key, binding) = try signedRecordSample()
        for length in 0..<encoded.count {
            XCTAssertThrowsError(try VaultUnverifiedRecord.parse(encoded.prefix(length), maxPayloadBytes: 4))
        }
        for index in encoded.indices {
            var changed = encoded
            changed[index] ^= 1
            let before = changed
            XCTAssertThrowsError(try VaultUnverifiedRecord.parse(changed, maxPayloadBytes: 4).verify(expected: binding, trustedPublicKey: key))
            XCTAssertEqual(changed, before)
        }
    }

    func testSignedRecordRejectsNoncanonicalAndOversizedInput() throws {
        let (encoded, _, _) = try signedRecordSample()
        var noncanonical = encoded
        noncanonical.replaceSubrange(2..<3, with: [0x18, 1])
        XCTAssertThrowsError(try VaultUnverifiedRecord.parse(noncanonical, maxPayloadBytes: 4)) {
            XCTAssertEqual($0 as? VaultRecordError, .nonCanonical)
        }
        var bomb = encoded
        bomb.replaceSubrange(6..<7, with: [0x5b, 255, 255, 255, 255, 255, 255, 255, 255])
        XCTAssertThrowsError(try VaultUnverifiedRecord.parse(bomb, maxPayloadBytes: 4)) {
            XCTAssertEqual($0 as? VaultRecordError, .sizeLimitExceeded)
        }
        var version = encoded
        version[2] = 2
        XCTAssertThrowsError(try VaultUnverifiedRecord.parse(version, maxPayloadBytes: 4)) {
            XCTAssertEqual($0 as? VaultRecordError, .unsupportedVersion)
        }
        for limit in [-1, 3, Int.max] {
            XCTAssertThrowsError(try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: limit)) {
                XCTAssertEqual($0 as? VaultRecordError, .sizeLimitExceeded)
            }
        }
        XCTAssertThrowsError(try VaultUnverifiedRecord.parse(encoded + Data([0]), maxPayloadBytes: 4)) {
            XCTAssertEqual($0 as? VaultRecordError, .malformed)
        }
    }

    func testSignedRecordWriterValidatesLengthsAndEpoch() throws {
        let (_, _, binding) = try signedRecordSample()
        let revision = Data(repeating: 5, count: 16)
        var wrong = Array(repeating: binding, count: 5)
        wrong[0].vaultId = Data()
        wrong[1].generation = Data(repeating: 0, count: 17)
        wrong[2].objectId = Data(repeating: 0, count: 15)
        wrong[3].authorId = Data()
        wrong[4].membershipHash = Data(repeating: 0, count: 31)
        for context in wrong {
            XCTAssertThrowsError(try VaultRecordCodec.signingBytes(binding: context, revisionId: revision, payload: Data(), maxPayloadBytes: 0)) {
                XCTAssertEqual($0 as? VaultRecordError, .malformed)
            }
        }
        XCTAssertThrowsError(try VaultRecordCodec.encodeSigned(binding: binding, revisionId: revision, payload: Data(), signature: Data(), maxPayloadBytes: 0)) {
            XCTAssertEqual($0 as? VaultRecordError, .malformed)
        }
        XCTAssertThrowsError(try VaultRecordCodec.signingBytes(binding: binding, revisionId: Data(), payload: Data(), maxPayloadBytes: 0)) {
            XCTAssertEqual($0 as? VaultRecordError, .malformed)
        }
        var zeroEpoch = binding
        zeroEpoch.epoch = 0
        XCTAssertThrowsError(try VaultRecordCodec.signingBytes(binding: zeroEpoch, revisionId: revision, payload: Data(), maxPayloadBytes: 0)) {
            XCTAssertEqual($0 as? VaultRecordError, .invalidEpoch)
        }
    }

    func testRejectsMalformedInputsBeforeOpening() {
        let nonce = Data(repeating: 0, count: 12)
        let key = SymmetricKey(size: .bits256)
        let tag = Data(repeating: 0, count: 16)
        for count in [0, 16, 24, 31, 33] {
            XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
                key: SymmetricKey(data: Data(repeating: 0, count: count)),
                nonce: nonce, aad: Data(), ciphertextAndTag: tag, maxPlaintextBytes: 0
            )) { XCTAssertEqual($0 as? VaultCryptoError, .invalidKeyLength) }
        }
        for count in [0, 8, 11, 13, 16] {
            XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
                key: key, nonce: Data(repeating: 0, count: count), aad: Data(),
                ciphertextAndTag: tag, maxPlaintextBytes: 0
            )) { XCTAssertEqual($0 as? VaultCryptoError, .invalidNonceLength) }
        }
        for count in 0..<16 {
            XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
                key: key, nonce: nonce, aad: Data(),
                ciphertextAndTag: tag.prefix(count), maxPlaintextBytes: 0
            )) { XCTAssertEqual($0 as? VaultCryptoError, .invalidCiphertextLength) }
        }
    }

    func testOpenAuthenticatesAndPreservesSlicedInput() throws {
        let key = SymmetricKey(size: .bits256)
        let aad = Data("synthetic context".utf8)
        let plaintext = Data([0, 255, 128, 10, 13])
        let sealed = try AES.GCM.seal(plaintext, using: key, authenticating: aad)
        let nonce = sealed.nonce.withUnsafeBytes { Data($0) }
        let input = (Data([255]) + sealed.ciphertext + sealed.tag).dropFirst()
        XCTAssertEqual(try VaultCrypto.openAES256GCM(
            key: key, nonce: nonce, aad: aad,
            ciphertextAndTag: input, maxPlaintextBytes: plaintext.count
        ), plaintext)
        XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
            key: key, nonce: nonce, aad: aad,
            ciphertextAndTag: input, maxPlaintextBytes: plaintext.count - 1
        )) { XCTAssertEqual($0 as? VaultCryptoError, .sizeLimitExceeded) }
        XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
            key: key, nonce: nonce, aad: aad + Data([0]),
            ciphertextAndTag: input, maxPlaintextBytes: plaintext.count
        )) { XCTAssertEqual($0 as? VaultCryptoError, .authenticationFailed) }
        var damaged = input
        damaged[damaged.startIndex] ^= 1
        let before = damaged
        XCTAssertThrowsError(try VaultCrypto.openAES256GCM(
            key: key, nonce: nonce, aad: aad,
            ciphertextAndTag: damaged, maxPlaintextBytes: plaintext.count
        )) { XCTAssertEqual($0 as? VaultCryptoError, .authenticationFailed) }
        XCTAssertEqual(damaged, before)
        XCTAssertEqual(input, sealed.ciphertext + sealed.tag)
    }

    func testSignatureVerificationRejectsChangedMessageAndMalformedInputs() throws {
        let signer = Curve25519.Signing.PrivateKey()
        let key = signer.publicKey.rawRepresentation
        let message = Data("synthetic signature fixture".utf8)
        let signature = try signer.signature(for: message)
        XCTAssertNoThrow(try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: signature))
        XCTAssertThrowsError(try VaultCrypto.verifyEd25519(publicKey: key, message: Data(), signature: signature)) {
            XCTAssertEqual($0 as? VaultCryptoError, .authenticationFailed)
        }
        XCTAssertThrowsError(try VaultCrypto.verifyEd25519(publicKey: key.dropLast(), message: message, signature: signature)) {
            XCTAssertEqual($0 as? VaultCryptoError, .invalidKeyLength)
        }
        XCTAssertThrowsError(try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: signature.dropLast())) {
            XCTAssertEqual($0 as? VaultCryptoError, .invalidSignatureLength)
        }
    }

    func testHKDFBoundsAndDomainSeparation() throws {
        let key = SymmetricKey(size: .bits256)
        let info = Data("synthetic info".utf8)
        let derived = try VaultCrypto.hkdfSHA256(inputKeyMaterial: key, salt: Data(), info: info, outputByteCount: 32)
        XCTAssertEqual(derived, try VaultCrypto.hkdfSHA256(inputKeyMaterial: key, salt: Data(), info: info, outputByteCount: 32))
        XCTAssertNotEqual(derived, try VaultCrypto.hkdfSHA256(inputKeyMaterial: key, salt: Data(), info: info + Data([0]), outputByteCount: 32))
        for count in [-1, 0, 8161, Int.max] {
            XCTAssertThrowsError(try VaultCrypto.hkdfSHA256(inputKeyMaterial: key, salt: Data(), info: info, outputByteCount: count)) {
                XCTAssertEqual($0 as? VaultCryptoError, .invalidOutputLength)
            }
        }
        for count in [1, 32, 8160] {
            XCTAssertEqual(try VaultCrypto.hkdfSHA256(inputKeyMaterial: key, salt: Data(), info: info, outputByteCount: count).bitCount, count * 8)
        }
    }
}
