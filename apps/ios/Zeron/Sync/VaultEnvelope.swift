import CryptoKit
import Foundation

/// Keyrings, key envelopes, and the recovery kit (RFC 0001 §5, §7.8) — the
/// Swift twins of `keyring.rs`, `envelope.rs`, and `recovery.rs`. Keyring
/// envelopes are opened with CryptoKit's HPKE (X25519 / HKDF-SHA256 /
/// AES-256-GCM, base mode); object keys unwrap under an epoch key with the
/// same per-record-derived-key construction content records use.
enum VaultEnvelopeError: Error, Equatable {
    case record(VaultRecordError)
    case malformed
    case unsupportedVersion
    case unsupportedFormat
    case wrongKind
    case wrongRecipient
    case wrongScope
    case duplicateEpoch
    case invalidEpoch
    case tooManyEpochs
    case sizeLimitExceeded
    case cryptographyFailed
}

/// The workspace keyring: one 32-byte wrapping key per write epoch.
struct VaultKeyring: Equatable {
    static let maxEpochs = 1024
    static let maxBytes = 16 + maxEpochs * 44

    private(set) var epochs: [UInt64: Data] = [:]

    init() {}

    mutating func insert(epoch: UInt64, key: Data) throws {
        guard epoch > 0 else { throw VaultEnvelopeError.invalidEpoch }
        guard key.count == 32 else { throw VaultEnvelopeError.malformed }
        if let existing = epochs[epoch] {
            guard existing == key else { throw VaultEnvelopeError.duplicateEpoch }
            return
        }
        guard epochs.count < Self.maxEpochs else { throw VaultEnvelopeError.tooManyEpochs }
        epochs[epoch] = key
    }

    mutating func merge(_ other: VaultKeyring) throws {
        for (epoch, key) in other.epochs { try insert(epoch: epoch, key: key) }
    }

    func epochKey(_ epoch: UInt64) -> Data? { epochs[epoch] }
    var latestEpoch: UInt64? { epochs.keys.max() }

    func encode() -> Data {
        var out = Data(capacity: 16 + epochs.count * 44)
        VaultRecordCodec.argument(into: &out, major: 5, value: 2)
        VaultRecordCodec.uintField(into: &out, key: 0, value: 1)
        VaultRecordCodec.argument(into: &out, major: 0, value: 1)
        VaultRecordCodec.argument(into: &out, major: 4, value: UInt64(epochs.count))
        for epoch in epochs.keys.sorted() {
            VaultRecordCodec.argument(into: &out, major: 4, value: 2)
            VaultRecordCodec.argument(into: &out, major: 0, value: epoch)
            VaultRecordCodec.argument(into: &out, major: 2, value: 32)
            out.append(epochs[epoch]!)
        }
        return out
    }

    static func decode(_ bytes: Data) throws -> VaultKeyring {
        guard bytes.count <= maxBytes else { throw VaultEnvelopeError.tooManyEpochs }
        return try VaultEnvelope.checked {
            var reader = VaultRecordReader(bytes)
            guard try reader.argument(major: 5) == 2 else { throw VaultEnvelopeError.malformed }
            guard try reader.uintField(0) == 1 else { throw VaultEnvelopeError.unsupportedVersion }
            guard try reader.argument(major: 0) == 1 else { throw VaultEnvelopeError.malformed }
            let count = try reader.argument(major: 4)
            guard count <= UInt64(maxEpochs) else { throw VaultEnvelopeError.tooManyEpochs }
            var keyring = VaultKeyring()
            var previous: UInt64 = 0
            for _ in 0..<count {
                guard try reader.argument(major: 4) == 2 else { throw VaultEnvelopeError.malformed }
                let epoch = try reader.argument(major: 0)
                guard epoch > previous else { throw VaultEnvelopeError.malformed }
                previous = epoch
                keyring.epochs[epoch] = try reader.fixedBytes(32)
            }
            guard reader.isAtEnd else { throw VaultEnvelopeError.malformed }
            return keyring
        }
    }
}

enum VaultRecipientKind: UInt64 {
    case device = 1, recovery = 2, epoch = 3
}

enum VaultEnvelope {
    static let maxPayloadBytes = VaultKeyring.maxBytes + 16 + 128
    private static let keyringInfoDomain = Data("zeron/keyring-envelope/v1\0".utf8)
    private static let objectKeyDomain = Data("zeron/object-key/v1\0".utf8)
    private static let objectAadDomain = Data("zeron/object-key/aad/v1\0".utf8)
    private static let hpkeSuite = HPKE.Ciphersuite(kem: .Curve25519_HKDF_SHA256, kdf: .HKDF_SHA256, aead: .AES_GCM_256)

    static func epochRecipientId(_ epoch: UInt64) -> Data {
        var out = Data(repeating: 0, count: 8)
        var bigEndian = epoch.bigEndian
        withUnsafeBytes(of: &bigEndian) { out.append(contentsOf: $0) }
        return out
    }

    private struct ParsedPayload {
        let recipientKind: VaultRecipientKind
        let recipientId: Data
        let encapsulation: Data
        let ciphertext: Data
    }

    private static func parsePayload(_ payload: Data, maxPlaintext: Int) throws -> ParsedPayload {
        var reader = VaultRecordReader(payload)
        guard try reader.argument(major: 5) == 5, try reader.uintField(0) == 1 else {
            throw VaultEnvelopeError.unsupportedFormat
        }
        guard let kind = VaultRecipientKind(rawValue: try reader.uintField(1)) else {
            throw VaultEnvelopeError.unsupportedFormat
        }
        let recipientId = try reader.fixedField(2, count: 16)
        let encapsulation = try reader.fixedField(3, count: 32)
        let ciphertext = try reader.bytesField(4, limit: maxPlaintext + 16)
        guard reader.isAtEnd, ciphertext.count >= 16 else { throw VaultEnvelopeError.unsupportedFormat }
        return ParsedPayload(recipientKind: kind, recipientId: recipientId, encapsulation: encapsulation, ciphertext: ciphertext)
    }

    private static func header(count: UInt64, kind: VaultRecipientKind, recipientId: Data) -> Data {
        var out = Data(capacity: 96)
        VaultRecordCodec.argument(into: &out, major: 5, value: count)
        VaultRecordCodec.uintField(into: &out, key: 0, value: 1)
        VaultRecordCodec.uintField(into: &out, key: 1, value: kind.rawValue)
        VaultRecordCodec.bytesField(into: &out, key: 2, value: recipientId)
        return out
    }

    /// Verify and open a keyring envelope addressed to this recipient.
    static func openKeyring(
        _ encoded: Data, expected: VaultRecordBinding, recipientKind: VaultRecipientKind,
        recipientId: Data, recipientKey: Curve25519.KeyAgreement.PrivateKey, trustedPublicKey: Data
    ) throws -> VaultKeyring {
        try checked {
            guard expected.kind == .keyEnvelope else { throw VaultEnvelopeError.wrongKind }
            let record = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: maxPayloadBytes)
                .verify(expected: expected, trustedPublicKey: trustedPublicKey)
            let parsed = try parsePayload(record.payload, maxPlaintext: VaultKeyring.maxBytes + 16)
            guard parsed.recipientKind == recipientKind, parsed.recipientId == recipientId,
                  parsed.recipientKind != .epoch else { throw VaultEnvelopeError.wrongRecipient }
            let context = try VaultRecordCodec.contextBytes(binding: expected, revisionId: record.revisionId)
            let info = keyringInfoDomain + context + header(count: 3, kind: parsed.recipientKind, recipientId: parsed.recipientId)
            var recipient = try HPKE.Recipient(privateKey: recipientKey, ciphersuite: hpkeSuite,
                                               info: info, encapsulatedKey: parsed.encapsulation)
            let plaintext = try recipient.open(parsed.ciphertext, authenticating: Data())
            return try VaultKeyring.decode(plaintext)
        }
    }

    /// Verify and unwrap an object key envelope with the epoch key named by
    /// the expected binding.
    static func unwrapObjectKey(
        _ encoded: Data, expected: VaultRecordBinding, epochKey: Data, trustedPublicKey: Data
    ) throws -> VaultContentKey {
        try checked {
            guard expected.kind == .keyEnvelope else { throw VaultEnvelopeError.wrongKind }
            guard epochKey.count == 32 else { throw VaultEnvelopeError.malformed }
            let record = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: maxPayloadBytes)
                .verify(expected: expected, trustedPublicKey: trustedPublicKey)
            let parsed = try parsePayload(record.payload, maxPlaintext: 48)
            guard parsed.recipientKind == .epoch, parsed.recipientId == epochRecipientId(expected.epoch) else {
                throw VaultEnvelopeError.wrongRecipient
            }
            let context = try VaultRecordCodec.contextBytes(binding: expected, revisionId: record.revisionId)
            var prefix = header(count: 5, kind: .epoch, recipientId: parsed.recipientId)
            VaultRecordCodec.bytesField(into: &prefix, key: 3, value: parsed.encapsulation)
            prefix[prefix.startIndex] = 0xa4 // fields 0..3 as a length-4 map
            let derived = try VaultCrypto.hkdfSHA256(
                inputKeyMaterial: SymmetricKey(data: epochKey), salt: parsed.encapsulation,
                info: objectKeyDomain + context + prefix, outputByteCount: 32
            )
            let plaintext = try VaultCrypto.openAES256GCM(
                key: derived, nonce: Data(repeating: 0, count: 12), aad: objectAadDomain + context + prefix,
                ciphertextAndTag: parsed.ciphertext, maxPlaintextBytes: 48
            )
            guard plaintext.count == 48 else { throw VaultEnvelopeError.unsupportedFormat }
            return try VaultContentKey(scope: VaultKeyScope(expected), identifier: plaintext.prefix(16),
                                       bytes: plaintext.suffix(32))
        }
    }

    fileprivate static func checked<Value>(_ operation: () throws -> Value) throws -> Value {
        do { return try operation() }
        catch let error as VaultEnvelopeError { throw error }
        catch let error as VaultRecordError { throw VaultEnvelopeError.record(error) }
        catch let error as VaultContentError { _ = error; throw VaultEnvelopeError.wrongScope }
        catch { throw VaultEnvelopeError.cryptographyFailed }
    }
}

enum VaultRecoveryError: Error, Equatable {
    case invalidCharacter
    case invalidLength
    case checksumMismatch
    case derivationFailed
}

/// The recovery secret and its kit text (RFC §4.1; plan Q4-A).
struct VaultRecoverySecret {
    private static let kitDomain = Data("zeron/recovery-kit/v1\0".utf8)
    private static let signingLabel = Data("zeron/recovery/sign/v1".utf8)
    private static let encryptionLabel = Data("zeron/recovery/hpke/v1".utf8)
    private static let alphabet = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".utf8)
    private static let kitSymbols = 55

    let secret: Data

    init(secret: Data) throws {
        guard secret.count == 32 else { throw VaultRecoveryError.invalidLength }
        self.secret = secret
    }

    private var checksum: Data { Data(SHA256.hash(data: Self.kitDomain + secret).prefix(2)) }

    var kit: String {
        let symbols = Self.base32Encode(secret + checksum)
        var text = ""
        for (index, symbol) in symbols.enumerated() {
            if index > 0, index % 5 == 0 { text.append("-") }
            text.append(Character(UnicodeScalar(symbol)))
        }
        return text
    }

    init(kit: String) throws {
        var symbols: [UInt8] = []
        for scalar in kit.unicodeScalars {
            if scalar.properties.isWhitespace || scalar == "-" { continue }
            guard scalar.isASCII else { throw VaultRecoveryError.invalidCharacter }
            symbols.append(UInt8(ascii: Unicode.Scalar(String(scalar).uppercased())!))
        }
        guard symbols.count == Self.kitSymbols else { throw VaultRecoveryError.invalidLength }
        let payload = try Self.base32Decode(symbols)
        let secret = payload.prefix(32)
        try self.init(secret: Data(secret))
        guard payload.suffix(from: payload.startIndex + 32).prefix(2) == checksum else {
            throw VaultRecoveryError.checksumMismatch
        }
    }

    func signingKey() throws -> Curve25519.Signing.PrivateKey {
        let seed = try VaultCrypto.hkdfSHA256(inputKeyMaterial: SymmetricKey(data: secret), salt: Data(),
                                              info: Self.signingLabel, outputByteCount: 32)
        do { return try Curve25519.Signing.PrivateKey(rawRepresentation: seed.withUnsafeBytes { Data($0) }) }
        catch { throw VaultRecoveryError.derivationFailed }
    }

    func encryptionKey() throws -> Curve25519.KeyAgreement.PrivateKey {
        let seed = try VaultCrypto.hkdfSHA256(inputKeyMaterial: SymmetricKey(data: secret), salt: Data(),
                                              info: Self.encryptionLabel, outputByteCount: 32)
        do { return try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: seed.withUnsafeBytes { Data($0) }) }
        catch { throw VaultRecoveryError.derivationFailed }
    }

    func authorityId() throws -> Data {
        VaultPolicy.recoveryAuthorityId(recoverySigningKey: try signingKey().publicKey.rawRepresentation)
    }

    private static func base32Encode(_ bytes: Data) -> [UInt8] {
        var out: [UInt8] = []
        var buffer: UInt32 = 0
        var bits = 0
        for byte in bytes {
            buffer = (buffer << 8) | UInt32(byte)
            bits += 8
            while bits >= 5 {
                bits -= 5
                out.append(alphabet[Int((buffer >> UInt32(bits)) & 31)])
            }
        }
        if bits > 0 { out.append(alphabet[Int((buffer << UInt32(5 - bits)) & 31)]) }
        return out
    }

    private static func base32Decode(_ symbols: [UInt8]) throws -> Data {
        var out = Data()
        var buffer: UInt32 = 0
        var bits = 0
        for symbol in symbols {
            guard let value = alphabet.firstIndex(of: symbol) else { throw VaultRecoveryError.invalidCharacter }
            buffer = (buffer << 5) | UInt32(value)
            bits += 5
            if bits >= 8 {
                bits -= 8
                out.append(UInt8((buffer >> UInt32(bits)) & 0xff))
            }
        }
        if bits > 0, buffer & ((1 << UInt32(bits)) - 1) != 0 { throw VaultRecoveryError.invalidCharacter }
        return out
    }
}
