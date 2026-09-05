import CryptoKit
import Foundation
import Security

enum VaultContentError: Error, Equatable {
    case record(VaultRecordError)
    case crypto(VaultCryptoError)
    case invalidKey
    case invalidSigningKey
    case wrongScope
    case wrongAuthor
    case wrongKind
    case wrongKey
    case wrongPurpose
    case unsupportedFormat
    case unsupportedSuite
    case unsupportedPurpose
    case sizeLimitExceeded
    case entropyUnavailable
    case cryptographyFailed
}

enum VaultContentPurpose: UInt64 {
    case chatUpdate = 1, checkpoint = 2, frontier = 3, registryField = 4
    case tail = 5, diff = 6, blob = 7, deviceSidecar = 8
}

struct VaultKeyScope: Equatable {
    let vaultId: Data
    let generation: Data
    let epoch: UInt64
    let objectId: Data

    init(_ binding: VaultRecordBinding) {
        vaultId = binding.vaultId
        generation = binding.generation
        epoch = binding.epoch
        objectId = binding.objectId
    }
}

final class VaultContentKey: CustomStringConvertible, CustomDebugStringConvertible {
    let scope: VaultKeyScope
    let identifier: Data
    fileprivate let material: SymmetricKey

    init(scope: VaultKeyScope, identifier: Data, bytes: Data) throws {
        guard scope.epoch > 0, [scope.vaultId, scope.generation, scope.objectId].allSatisfy({ $0.count == 16 }) else {
            throw VaultContentError.wrongScope
        }
        guard identifier.count == 16, bytes.count == 32 else { throw VaultContentError.invalidKey }
        self.scope = scope
        self.identifier = identifier
        material = SymmetricKey(data: bytes)
    }

    static func generate(scope: VaultKeyScope) throws -> VaultContentKey {
        guard scope.epoch > 0, [scope.vaultId, scope.generation, scope.objectId].allSatisfy({ $0.count == 16 }) else {
            throw VaultContentError.wrongScope
        }
        let identifier = try VaultContentCrypto.randomBytes(16)
        var secret = try VaultContentCrypto.randomBytes(32)
        defer { secret.resetBytes(in: secret.startIndex..<secret.endIndex) }
        return try VaultContentKey(scope: scope, identifier: identifier, bytes: secret)
    }

    func exposeSecret() -> Data { material.withUnsafeBytes { Data($0) } }
    var description: String { "ContentKey([REDACTED])" }
    var debugDescription: String { description }
}

final class VaultDeviceSigner: CustomStringConvertible, CustomDebugStringConvertible {
    let authorId: Data
    fileprivate let key: Curve25519.Signing.PrivateKey

    init(authorId: Data, seed: Data) throws {
        guard authorId.count == 16, seed.count == 32 else { throw VaultContentError.invalidSigningKey }
        self.authorId = authorId
        do { key = try Curve25519.Signing.PrivateKey(rawRepresentation: seed) }
        catch { throw VaultContentError.invalidSigningKey }
        guard VaultCrypto.passesEd25519PointEncodingPrecheck(key.publicKey.rawRepresentation) else {
            throw VaultContentError.invalidSigningKey
        }
    }

    var publicKey: Data { key.publicKey.rawRepresentation }
    var description: String { "DeviceSigner([REDACTED])" }
    var debugDescription: String { description }
}

struct VaultSealedContent: CustomStringConvertible, CustomDebugStringConvertible {
    let binding: VaultRecordBinding
    let revisionId: Data
    let encoded: Data
    fileprivate init(binding: VaultRecordBinding, revisionId: Data, encoded: Data) {
        self.binding = binding
        self.revisionId = revisionId
        self.encoded = encoded
    }
    var description: String { "SealedContent([REDACTED])" }
    var debugDescription: String { description }
}

struct VaultOpenedContent: CustomStringConvertible, CustomDebugStringConvertible {
    let revisionId: Data
    let plaintext: Data
    fileprivate init(revisionId: Data, plaintext: Data) {
        self.revisionId = revisionId
        self.plaintext = plaintext
    }
    var description: String { "OpenedContent([REDACTED])" }
    var debugDescription: String { description }
}

enum VaultContentCrypto {
    static let maxPlaintextBytes = 16 * 1024 * 1024 - 400
    private static let payloadOverhead = 144
    private static let keyDomain = Data("zeron/content/key/v1\0".utf8)
    private static let aadDomain = Data("zeron/content/aad/v1\0".utf8)

    static func seal(
        binding: VaultRecordBinding, purpose: VaultContentPurpose, key: VaultContentKey,
        signer: VaultDeviceSigner, plaintext: Data, maxPlaintextBytes: Int
    ) throws -> VaultSealedContent {
        try checked {
            let limit = try payloadLimit(maxPlaintextBytes)
            try checkScope(binding, key: key)
            guard binding.authorId == signer.authorId else { throw VaultContentError.wrongAuthor }
            guard plaintext.count <= maxPlaintextBytes else { throw VaultContentError.sizeLimitExceeded }
            guard binding.membershipHash.count == 32 else { throw VaultRecordError.malformed }
            let material = try randomBytes(48)
            let revisionId = material.prefix(16)
            let salt = material.suffix(32)
            let header = protectedHeader(count: 5, purpose: purpose, identifier: key.identifier, salt: salt)
            let context = try VaultRecordCodec.contextBytes(binding: binding, revisionId: revisionId)
            let derived = try VaultCrypto.hkdfSHA256(inputKeyMaterial: key.material, salt: salt, info: keyDomain + context + header, outputByteCount: 32)
            let box = try AES.GCM.seal(plaintext, using: derived, nonce: AES.GCM.Nonce(data: Data(repeating: 0, count: 12)), authenticating: aadDomain + context + header)
            var payload = protectedHeader(count: 6, purpose: purpose, identifier: key.identifier, salt: salt)
            VaultRecordCodec.bytesField(into: &payload, key: 5, value: box.ciphertext + box.tag)
            let input = try VaultRecordCodec.signingBytes(binding: binding, revisionId: revisionId, payload: payload, maxPayloadBytes: limit)
            let signature = try signer.key.signature(for: input)
            let encoded = try VaultRecordCodec.encodeSigned(binding: binding, revisionId: revisionId, payload: payload, signature: signature, maxPayloadBytes: limit)
            return VaultSealedContent(binding: binding, revisionId: revisionId, encoded: encoded)
        }
    }

    static func open(
        _ encoded: Data, expected: VaultRecordBinding, purpose: VaultContentPurpose,
        key: VaultContentKey, trustedPublicKey: Data, maxPlaintextBytes: Int
    ) throws -> VaultOpenedContent {
        try checked {
            let limit = try payloadLimit(maxPlaintextBytes)
            try checkScope(expected, key: key)
            let record = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: limit).verify(expected: expected, trustedPublicKey: trustedPublicKey)
            var reader = VaultRecordReader(record.payload)
            guard try reader.argument(major: 5) == 6, try reader.uintField(0) == 1 else { throw VaultContentError.unsupportedFormat }
            guard try reader.uintField(1) == 1 else { throw VaultContentError.unsupportedSuite }
            guard let storedPurpose = VaultContentPurpose(rawValue: try reader.uintField(2)) else { throw VaultContentError.unsupportedPurpose }
            guard storedPurpose == purpose else { throw VaultContentError.wrongPurpose }
            let identifier = try reader.fixedField(3, count: 16)
            guard identifier == key.identifier else { throw VaultContentError.wrongKey }
            let salt = try reader.fixedField(4, count: 32)
            let ciphertext = try reader.bytesField(5, limit: maxPlaintextBytes + 16)
            guard reader.isAtEnd else { throw VaultRecordError.malformed }
            guard ciphertext.count >= 16 else { throw VaultContentError.unsupportedFormat }
            let header = protectedHeader(count: 5, purpose: purpose, identifier: identifier, salt: salt)
            let context = try VaultRecordCodec.contextBytes(binding: expected, revisionId: record.revisionId)
            let derived = try VaultCrypto.hkdfSHA256(inputKeyMaterial: key.material, salt: salt, info: keyDomain + context + header, outputByteCount: 32)
            let plaintext = try VaultCrypto.openAES256GCM(key: derived, nonce: Data(repeating: 0, count: 12), aad: aadDomain + context + header, ciphertextAndTag: ciphertext, maxPlaintextBytes: maxPlaintextBytes)
            return VaultOpenedContent(revisionId: record.revisionId, plaintext: plaintext)
        }
    }

    private static func checkScope(_ binding: VaultRecordBinding, key: VaultContentKey) throws {
        guard binding.kind == .content else { throw VaultContentError.wrongKind }
        guard key.scope == VaultKeyScope(binding) else { throw VaultContentError.wrongScope }
    }

    private static func payloadLimit(_ maximum: Int) throws -> Int {
        guard maximum >= 0, maximum <= maxPlaintextBytes else { throw VaultContentError.sizeLimitExceeded }
        return maximum + payloadOverhead
    }

    private static func protectedHeader(count: UInt64, purpose: VaultContentPurpose, identifier: Data, salt: Data) -> Data {
        var header = Data(capacity: 128)
        VaultRecordCodec.argument(into: &header, major: 5, value: count)
        VaultRecordCodec.uintField(into: &header, key: 0, value: 1)
        VaultRecordCodec.uintField(into: &header, key: 1, value: 1)
        VaultRecordCodec.uintField(into: &header, key: 2, value: purpose.rawValue)
        VaultRecordCodec.bytesField(into: &header, key: 3, value: identifier)
        VaultRecordCodec.bytesField(into: &header, key: 4, value: salt)
        return header
    }

    fileprivate static func randomBytes(_ count: Int) throws -> Data {
        var bytes = Data(count: count)
        let status = bytes.withUnsafeMutableBytes { buffer -> OSStatus in
            guard let address = buffer.baseAddress else { return errSecParam }
            return SecRandomCopyBytes(kSecRandomDefault, count, address)
        }
        guard status == errSecSuccess else { throw VaultContentError.entropyUnavailable }
        return bytes
    }

    private static func checked<Value>(_ operation: () throws -> Value) throws -> Value {
        do { return try operation() }
        catch let error as VaultContentError { throw error }
        catch let error as VaultRecordError { throw VaultContentError.record(error) }
        catch let error as VaultCryptoError { throw VaultContentError.crypto(error) }
        catch { throw VaultContentError.cryptographyFailed }
    }
}
