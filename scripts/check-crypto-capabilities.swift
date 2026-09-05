import CryptoKit
import Foundation

struct ProbeFailure: Error {
    let check: String
}

func check(_ name: String, _ condition: Bool) throws {
    guard condition else { throw ProbeFailure(check: name) }
    print("PASS: \(name)")
}

func rejects(_ name: String, _ expected: VaultCryptoError, _ operation: () throws -> Void) throws {
    do {
        try operation()
    } catch let error as VaultCryptoError where error == expected {
        return
    }
    throw ProbeFailure(check: name)
}

func hex(_ value: String) throws -> Data {
    let bytes = Array(value.utf8)
    guard bytes.count.isMultiple(of: 2) else { throw ProbeFailure(check: "odd hex length") }
    return try Data(stride(from: 0, to: bytes.count, by: 2).map { i in
        guard let byte = UInt8(String(decoding: bytes[i...i + 1], as: UTF8.self), radix: 16) else {
            throw ProbeFailure(check: "invalid fixture hex")
        }
        return byte
    })
}

struct PrimitiveVectors: Codable {
    let version: Int
    let aes256gcm: [AESVector]
    var ed25519: [SignatureVector]
    let hkdfSha256: [HKDFVector]
    var signedRecords: [SignedRecordVector]
    let recordMutations: [RecordMutation]
    let ed25519PointEncodings: [EncodingVector]
    let ed25519ScalarEncodings: [EncodingVector]
    let ed25519Rejections: [RejectionVector]
    var encryptedContent: [EncryptedContentVector]
}

struct EncryptedContentVector: Codable {
    let name, vaultId, generation, objectId, authorId, membershipHash: String
    let epoch, purpose: UInt64
    let keyId, contentKey, signerSeed, publicKey, plaintext, encoded: String
    var peerRecord: String?
}

struct EncodingVector: Codable {
    let name, encoding: String
    let allowed: Bool
}

struct RejectionVector: Codable {
    let name, publicKey, message, signature: String
}

struct AESVector: Codable {
    let name, key, nonce, aad, plaintext, ciphertext, tag: String
}

struct SignatureVector: Codable {
    let name, seed, publicKey, message, signature: String
    var peerSignature: String?
}

struct HKDFVector: Codable {
    let name, ikm, salt, info, output: String
}

struct SignedRecordVector: Codable {
    let name: String
    let kind, epoch: UInt64
    let vaultId, generation, objectId, authorId, revisionId, membershipHash: String
    let payload, seed, publicKey, signingBytes, signature: String
    var peerSignature: String?
    var peerRecord: String?
}

struct RecordMutation: Codable {
    let name: String
    let offset, remove: Int
    let insert, error: String
}

@main
struct CryptoCapabilityProbe {
    static func main() throws {
        let path = CommandLine.arguments.dropFirst().first ?? "crates/crypto/tests/fixtures/primitives.json"
        var vectors = try JSONDecoder().decode(PrimitiveVectors.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        try check("shared fixture version and nonempty groups", vectors.version == 1 && !vectors.aes256gcm.isEmpty && !vectors.ed25519.isEmpty && !vectors.hkdfSha256.isEmpty)
        for vector in vectors.aes256gcm { try aes(vector) }
        for index in vectors.ed25519.indices {
            vectors.ed25519[index].peerSignature = try signature(vectors.ed25519[index])
        }
        for vector in vectors.hkdfSha256 { try hkdf(vector) }
        try ed25519EncodingChecks(vectors)
        try check("signed-wrapper fixture groups", !vectors.signedRecords.isEmpty && vectors.recordMutations.count >= 21)
        for index in vectors.signedRecords.indices {
            let (signature, record) = try signedRecord(vectors.signedRecords[index], mutations: vectors.recordMutations)
            vectors.signedRecords[index].peerSignature = signature.map { String(format: "%02x", $0) }.joined()
            vectors.signedRecords[index].peerRecord = record.map { String(format: "%02x", $0) }.joined()
        }
        try check("encrypted-content fixtures", !vectors.encryptedContent.isEmpty)
        for index in vectors.encryptedContent.indices {
            let record = try encryptedContent(vectors.encryptedContent[index])
            vectors.encryptedContent[index].peerRecord = record.map { String(format: "%02x", $0) }.joined()
        }
        try hpkeCapability()
        if let outputPath = CommandLine.arguments.dropFirst(2).first {
            try JSONEncoder().encode(vectors).write(to: URL(fileURLWithPath: outputPath), options: .atomic)
            print("PASS: emitted public-key test vectors for Rust verification")
        }
        print("Core encryption conformance passed; complete sync integration and security review remain separate.")
    }

    static func aes(_ v: AESVector) throws {
        let key = SymmetricKey(data: try hex(v.key))
        let nonce = try hex(v.nonce)
        let aad = try hex(v.aad)
        let plaintext = try hex(v.plaintext)
        let ciphertext = try hex(v.ciphertext + v.tag)
        let opened = try VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
        try check("\(v.name): open shared bytes", opened == plaintext)
        let sealed = try AES.GCM.seal(plaintext, using: key, nonce: AES.GCM.Nonce(data: nonce), authenticating: aad)
        try check("\(v.name): Swift seals identical shared bytes", sealed.ciphertext + sealed.tag == ciphertext)
        for index in ciphertext.indices {
            var damaged = ciphertext
            damaged[index] ^= 1
            let before = damaged
            try rejects("changed ciphertext/tag", .authenticationFailed) {
                _ = try VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad, ciphertextAndTag: damaged, maxPlaintextBytes: plaintext.count)
            }
            guard damaged == before else { throw ProbeFailure(check: "input mutated") }
        }
        try rejects("changed AAD", .authenticationFailed) {
            _ = try VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad + Data([0]), ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
        }
        try rejects("wrong key", .authenticationFailed) {
            _ = try VaultCrypto.openAES256GCM(key: SymmetricKey(data: Data(repeating: 1, count: 32)), nonce: nonce, aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
        }
        try rejects("wrong nonce", .authenticationFailed) {
            _ = try VaultCrypto.openAES256GCM(key: key, nonce: Data(repeating: 1, count: 12), aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
        }
        try rejects("plaintext budget", .sizeLimitExceeded) {
            _ = try VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count - 1)
        }
        for length in [0, 16, 24, 31, 33] {
            try rejects("key length", .invalidKeyLength) {
                _ = try VaultCrypto.openAES256GCM(key: SymmetricKey(data: Data(repeating: 0, count: length)), nonce: nonce, aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
            }
        }
        for length in [0, 8, 11, 13, 16] {
            try rejects("nonce length", .invalidNonceLength) {
                _ = try VaultCrypto.openAES256GCM(key: key, nonce: Data(repeating: 0, count: length), aad: aad, ciphertextAndTag: ciphertext, maxPlaintextBytes: plaintext.count)
            }
        }
        for length in 0..<16 {
            try rejects("ciphertext length", .invalidCiphertextLength) {
                _ = try VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad, ciphertextAndTag: ciphertext.prefix(length), maxPlaintextBytes: plaintext.count)
            }
        }
        let sliced = (Data([255]) + ciphertext).dropFirst()
        try check("\(v.name): sliced Data and negative cases", VaultCrypto.openAES256GCM(key: key, nonce: nonce, aad: aad, ciphertextAndTag: sliced, maxPlaintextBytes: plaintext.count) == plaintext)
    }

    static func signature(_ v: SignatureVector) throws -> String {
        let key = try hex(v.publicKey)
        let message = try hex(v.message)
        let sig = try hex(v.signature)
        try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: sig)
        let signer = try Curve25519.Signing.PrivateKey(rawRepresentation: hex(v.seed))
        let generated = try signer.signature(for: message)
        try check("\(v.name): public key matches shared bytes", signer.publicKey.rawRepresentation == key)
        try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: generated)
        try rejects("changed message", .authenticationFailed) {
            try VaultCrypto.verifyEd25519(publicKey: key, message: message + Data([0]), signature: sig)
        }
        for index in sig.indices {
            var damaged = sig
            damaged[index] ^= 1
            try rejects("changed signature", .authenticationFailed) {
                try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: damaged)
            }
        }
        try rejects("wrong signer", .authenticationFailed) {
            try VaultCrypto.verifyEd25519(publicKey: Data(repeating: 0, count: 32), message: message, signature: sig)
        }
        for length in [0, 31, 33] {
            try rejects("public key length", .invalidKeyLength) {
                try VaultCrypto.verifyEd25519(publicKey: Data(repeating: 0, count: length), message: message, signature: sig)
            }
        }
        for length in [0, 63, 65] {
            try rejects("signature length", .invalidSignatureLength) {
                try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: Data(repeating: 0, count: length))
            }
        }
        print("PASS: \(v.name): verification and negative cases")
        return generated.map { String(format: "%02x", $0) }.joined()
    }

    static func hkdf(_ v: HKDFVector) throws {
        let ikm = SymmetricKey(data: try hex(v.ikm))
        let salt = try hex(v.salt)
        let info = try hex(v.info)
        let output = try hex(v.output)
        let derived = try VaultCrypto.hkdfSHA256(inputKeyMaterial: ikm, salt: salt, info: info, outputByteCount: output.count)
        try check("\(v.name): derive shared bytes", derived.withUnsafeBytes { Data($0) } == output)
        let separated = try VaultCrypto.hkdfSHA256(inputKeyMaterial: ikm, salt: salt, info: info + Data([0]), outputByteCount: output.count)
        try check("\(v.name): info-label separation", separated != derived)
        for length in [-1, 0, 8161, Int.max] {
            try rejects("HKDF output bounds", .invalidOutputLength) {
                _ = try VaultCrypto.hkdfSHA256(inputKeyMaterial: ikm, salt: salt, info: info, outputByteCount: length)
            }
        }
        for length in [1, 32, 8160] {
            let result = try VaultCrypto.hkdfSHA256(inputKeyMaterial: ikm, salt: salt, info: info, outputByteCount: length)
            guard result.bitCount == length * 8 else { throw ProbeFailure(check: "HKDF output size") }
        }
        print("PASS: \(v.name): output bounds")
    }

    static func ed25519EncodingChecks(_ fixtures: PrimitiveVectors) throws {
        guard fixtures.ed25519PointEncodings.count >= 13, fixtures.ed25519ScalarEncodings.count >= 9,
              fixtures.ed25519Rejections.count >= 8 else { throw ProbeFailure(check: "Ed25519 rejection fixtures missing") }
        for v in fixtures.ed25519PointEncodings {
            var encoded = (Data([255]) + (try hex(v.encoding))).dropFirst()
            let before = encoded
            guard VaultCrypto.passesEd25519PointEncodingPrecheck(encoded) == v.allowed, encoded == before else {
                throw ProbeFailure(check: v.name)
            }
            if encoded.count == 32 {
                encoded[encoded.index(before: encoded.endIndex)] ^= 0x80
                guard VaultCrypto.passesEd25519PointEncodingPrecheck(encoded) == v.allowed else {
                    throw ProbeFailure(check: "\(v.name): opposite sign")
                }
            }
        }
        for v in fixtures.ed25519ScalarEncodings {
            let encoded = (Data([255]) + (try hex(v.encoding))).dropFirst()
            guard VaultCrypto.passesEd25519ScalarEncodingPrecheck(encoded) == v.allowed else {
                throw ProbeFailure(check: v.name)
            }
        }
        for lowByte: UInt8 in 0xed...0xff {
            for highByte: UInt8 in [0x7f, 0xff] {
                var encoded = Data(repeating: 0xff, count: 32)
                encoded[0] = lowByte
                encoded[31] = highByte
                guard !VaultCrypto.passesEd25519PointEncodingPrecheck(encoded) else {
                    throw ProbeFailure(check: "noncanonical field coordinate accepted")
                }
            }
        }
        for v in fixtures.ed25519Rejections {
            let key = try hex(v.publicKey)
            let message = try hex(v.message)
            let signature = try hex(v.signature)
            guard key.count == 32, signature.count == 64 else { throw ProbeFailure(check: "\(v.name): invalid fixture length") }
            try rejects(v.name, .authenticationFailed) {
                try VaultCrypto.verifyEd25519(publicKey: key, message: message, signature: signature)
            }
        }
        print("PASS: shared Ed25519 encoding prechecks, sign variants, field/scalar bounds, and rejection vectors")
    }

    static func rejectsRecord(_ name: String, _ expected: VaultRecordError, _ operation: () throws -> Void) throws {
        do {
            try operation()
        } catch let error as VaultRecordError where error == expected {
            return
        }
        throw ProbeFailure(check: name)
    }

    static func signedRecord(_ v: SignedRecordVector, mutations: [RecordMutation]) throws -> (Data, Data) {
        guard let kind = VaultRecordKind(rawValue: v.kind) else { throw ProbeFailure(check: "fixture record kind") }
        let binding = try VaultRecordBinding(
            kind: kind, vaultId: hex(v.vaultId), generation: hex(v.generation), epoch: v.epoch,
            objectId: hex(v.objectId), authorId: hex(v.authorId), membershipHash: hex(v.membershipHash)
        )
        let revision = try hex(v.revisionId)
        let payload = try hex(v.payload)
        let signature = try hex(v.signature)
        let publicKey = try hex(v.publicKey)
        let input = try VaultRecordCodec.signingBytes(binding: binding, revisionId: revision, payload: payload, maxPayloadBytes: payload.count)
        try check("\(v.name): canonical signed bytes", input == hex(v.signingBytes))
        let encoded = try VaultRecordCodec.encodeSigned(binding: binding, revisionId: revision, payload: payload, signature: signature, maxPayloadBytes: payload.count)
        let expected = try Data([0xab]) + Data(hex(v.signingBytes).dropFirst(24)) + Data([0x0a, 0x58, 0x40]) + signature
        try check("\(v.name): canonical signed map", encoded == expected)
        let unverified = try VaultUnverifiedRecord.parse((Data([255]) + encoded).dropFirst(), maxPayloadBytes: payload.count)
        try check("\(v.name): unverified metadata and redaction", unverified.untrustedBinding == binding && unverified.untrustedRevisionId == revision && String(reflecting: unverified) == "UnverifiedRecord([REDACTED])")
        let verified = try unverified.verify(expected: binding, trustedPublicKey: publicKey)
        try check("\(v.name): verify Rust fixture with sliced input", verified.payload == payload && verified.revisionId == revision && verified.binding == binding && String(reflecting: verified) == "VerifiedRecord([REDACTED])")
        var wrong = binding
        wrong.vaultId[wrong.vaultId.startIndex] ^= 1
        try rejectsRecord("wrong trusted binding", .contextMismatch) {
            _ = try unverified.verify(expected: wrong, trustedPublicKey: publicKey)
        }
        for key in [Data(repeating: 0, count: 32), Data(repeating: 0, count: 31)] {
            try rejectsRecord("wrong trusted key", .invalidSignature) {
                _ = try unverified.verify(expected: binding, trustedPublicKey: key)
            }
        }
        for index in encoded.indices {
            var damaged = encoded
            damaged[index] ^= 1
            let before = damaged
            do {
                _ = try VaultUnverifiedRecord.parse(damaged, maxPayloadBytes: payload.count).verify(expected: binding, trustedPublicKey: publicKey)
            } catch is VaultRecordError {
                guard damaged == before else { throw ProbeFailure(check: "record input mutated") }
                continue
            }
            throw ProbeFailure(check: "record byte tampering accepted")
        }
        for length in 0..<encoded.count {
            do {
                _ = try VaultUnverifiedRecord.parse(encoded.prefix(length), maxPayloadBytes: payload.count)
            } catch is VaultRecordError {
                continue
            }
            throw ProbeFailure(check: "truncated record accepted")
        }
        for mutation in mutations {
            var damaged = encoded
            damaged.replaceSubrange(mutation.offset..<mutation.offset + mutation.remove, with: try hex(mutation.insert))
            do {
                _ = try VaultUnverifiedRecord.parse(damaged, maxPayloadBytes: payload.count)
            } catch let error as VaultRecordError {
                let name = String(describing: error)
                guard name.prefix(1).uppercased() + name.dropFirst() == mutation.error else {
                    throw ProbeFailure(check: "\(mutation.name): unexpected error \(error)")
                }
                continue
            }
            throw ProbeFailure(check: "\(mutation.name): malformed record accepted")
        }
        try rejectsRecord("trailing bytes", .malformed) {
            _ = try VaultUnverifiedRecord.parse(encoded + Data([0]), maxPayloadBytes: payload.count)
        }
        for limit in [-1, payload.count - 1, Int.max] {
            try rejectsRecord("record payload/overflow limit", .sizeLimitExceeded) {
                _ = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: limit)
            }
        }
        try rejectsRecord("encoded size cap", .sizeLimitExceeded) {
            _ = try VaultUnverifiedRecord.parse(Data(repeating: 0, count: 257), maxPayloadBytes: 0)
        }
        try rejectsRecord("writer payload cap", .sizeLimitExceeded) {
            _ = try VaultRecordCodec.signingBytes(binding: binding, revisionId: revision, payload: payload, maxPayloadBytes: payload.count - 1)
        }
        var zeroEpoch = binding
        zeroEpoch.epoch = 0
        try rejectsRecord("zero epoch", .invalidEpoch) {
            _ = try VaultRecordCodec.signingBytes(binding: zeroEpoch, revisionId: revision, payload: payload, maxPayloadBytes: payload.count)
        }
        print("PASS: \(v.name): byte tampering, truncation, shared malformed corpus, and limits")
        let signer = try Curve25519.Signing.PrivateKey(rawRepresentation: hex(v.seed))
        try check("\(v.name): fixture signer key", signer.publicKey.rawRepresentation == publicKey)
        for epoch: UInt64 in [1, 23, 24, 255, 256, 65535, 65536, UInt64(UInt32.max), 1 << 32, UInt64.max] {
            var context = binding
            context.epoch = epoch
            for length in [0, 23, 24, 255, 256, 65535, 65536] {
                let body = Data(repeating: 42, count: length)
                let bytes = try VaultRecordCodec.signingBytes(binding: context, revisionId: revision, payload: body, maxPayloadBytes: length)
                let sig = try signer.signature(for: bytes)
                let wire = try VaultRecordCodec.encodeSigned(binding: context, revisionId: revision, payload: body, signature: sig, maxPayloadBytes: length)
                let parsed = try VaultUnverifiedRecord.parse(wire, maxPayloadBytes: length).verify(expected: context, trustedPublicKey: publicKey)
                guard parsed.payload == body, parsed.binding.epoch == epoch else { throw ProbeFailure(check: "integer/length boundary") }
            }
        }
        print("PASS: \(v.name): UInt64 and byte-string length boundaries")
        let peerSignature = try signer.signature(for: input)
        let peerRecord = try VaultRecordCodec.encodeSigned(binding: binding, revisionId: revision, payload: payload, signature: peerSignature, maxPayloadBytes: payload.count)
        return (peerSignature, peerRecord)
    }

    static func encryptedContent(_ vector: EncryptedContentVector) throws -> Data {
        guard let purpose = VaultContentPurpose(rawValue: vector.purpose) else { throw ProbeFailure(check: "fixture purpose") }
        let binding = try VaultRecordBinding(
            kind: .content, vaultId: hex(vector.vaultId), generation: hex(vector.generation), epoch: vector.epoch,
            objectId: hex(vector.objectId), authorId: hex(vector.authorId), membershipHash: hex(vector.membershipHash)
        )
        let key = try VaultContentKey(scope: VaultKeyScope(binding), identifier: hex(vector.keyId), bytes: hex(vector.contentKey))
        let signer = try VaultDeviceSigner(authorId: binding.authorId, seed: hex(vector.signerSeed))
        try check("\(vector.name): fixture signing key", signer.publicKey == hex(vector.publicKey))
        let plaintext = try hex(vector.plaintext)
        let encoded = try hex(vector.encoded)
        let opened = try VaultContentCrypto.open((Data([255]) + encoded).dropFirst(), expected: binding, purpose: purpose, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: plaintext.count)
        try check("\(vector.name): Swift decrypts Rust ciphertext", opened.plaintext == plaintext)
        let sealed = try VaultContentCrypto.seal(binding: binding, purpose: purpose, key: key, signer: signer, plaintext: plaintext, maxPlaintextBytes: plaintext.count)
        let again = try VaultContentCrypto.seal(binding: binding, purpose: purpose, key: key, signer: signer, plaintext: plaintext, maxPlaintextBytes: plaintext.count)
        try check("\(vector.name): fresh record identity and ciphertext", sealed.revisionId != again.revisionId && sealed.encoded != again.encoded)
        try check("\(vector.name): native round trip", VaultContentCrypto.open(sealed.encoded, expected: binding, purpose: purpose, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: plaintext.count).plaintext == plaintext)
        if plaintext.count >= 16 {
            try check("\(vector.name): plaintext canary absent", sealed.encoded.range(of: plaintext) == nil)
        }
        for index in encoded.indices {
            var corrupted = encoded
            corrupted[index] ^= 1
            do {
                _ = try VaultContentCrypto.open(corrupted, expected: binding, purpose: purpose, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: plaintext.count)
            } catch is VaultContentError { continue }
            throw ProbeFailure(check: "encrypted record tampering accepted")
        }
        for length in 0..<encoded.count {
            do {
                _ = try VaultContentCrypto.open(encoded.prefix(length), expected: binding, purpose: purpose, key: key, trustedPublicKey: signer.publicKey, maxPlaintextBytes: plaintext.count)
            } catch is VaultContentError { continue }
            throw ProbeFailure(check: "encrypted record truncation accepted")
        }
        print("PASS: \(vector.name): tamper and truncation rejection")
        return sealed.encoded
    }

    static func hpkeCapability() throws {
        let suite = HPKE.Ciphersuite(kem: .Curve25519_HKDF_SHA256, kdf: .HKDF_SHA256, aead: .AES_GCM_256)
        let key = Curve25519.KeyAgreement.PrivateKey()
        let info = Data("synthetic HPKE capability probe; not enrollment".utf8)
        let aad = Data("synthetic associated data".utf8)
        let message = Data("synthetic capability probe".utf8)
        var sender = try HPKE.Sender(recipientKey: key.publicKey, ciphersuite: suite, info: info)
        let ciphertext = try sender.seal(message, authenticating: aad)
        var receiver = try HPKE.Recipient(privateKey: key, ciphersuite: suite, info: info, encapsulatedKey: sender.encapsulatedKey)
        try check("HPKE candidate suite round trip", receiver.open(ciphertext, authenticating: aad) == message)
        var damaged = ciphertext
        damaged[damaged.startIndex] ^= 1
        let cases = [
            (key, info, aad + Data([0]), ciphertext),
            (key, info + Data([0]), aad, ciphertext),
            (Curve25519.KeyAgreement.PrivateKey(), info, aad, ciphertext),
            (key, info, aad, damaged),
        ]
        for (privateKey, context, associatedData, body) in cases {
            do {
                var recipient = try HPKE.Recipient(privateKey: privateKey, ciphersuite: suite, info: context, encapsulatedKey: sender.encapsulatedKey)
                _ = try recipient.open(body, authenticating: associatedData)
            } catch {
                continue
            }
            throw ProbeFailure(check: "HPKE accepted invalid input")
        }
        print("PASS: HPKE rejects changed AAD/info/recipient/ciphertext")
    }
}
