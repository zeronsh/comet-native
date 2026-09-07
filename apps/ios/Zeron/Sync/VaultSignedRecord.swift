import Foundation

enum VaultRecordError: Error, Equatable {
    case malformed
    case nonCanonical
    case unsupportedVersion
    case unsupportedKind
    case invalidEpoch
    case sizeLimitExceeded
    case contextMismatch
    case invalidSignature
}

enum VaultRecordKind: UInt64 {
    case policy = 1
    case keyEnvelope = 2
    case content = 3
}

struct VaultRecordBinding: Equatable {
    var kind: VaultRecordKind
    var vaultId: Data
    var generation: Data
    var epoch: UInt64
    var objectId: Data
    var authorId: Data
    var membershipHash: Data
}

struct VaultUnverifiedRecord: CustomStringConvertible, CustomDebugStringConvertible {
    private let binding: VaultRecordBinding
    private let revisionId: Data
    private let payload: Data
    private let signature: Data
    private let maxPayloadBytes: Int

    var untrustedBinding: VaultRecordBinding { binding }
    var untrustedRevisionId: Data { revisionId }
    var description: String { "UnverifiedRecord([REDACTED])" }
    var debugDescription: String { description }

    static func parse(_ encoded: Data, maxPayloadBytes: Int) throws -> Self {
        guard encoded.count <= (try VaultRecordCodec.totalLimit(maxPayloadBytes)) else {
            throw VaultRecordError.sizeLimitExceeded
        }
        var reader = VaultRecordReader(encoded)
        guard try reader.argument(major: 5) == 11 else { throw VaultRecordError.malformed }
        guard try reader.uintField(0) == 1 else { throw VaultRecordError.unsupportedVersion }
        guard let kind = VaultRecordKind(rawValue: try reader.uintField(1)) else {
            throw VaultRecordError.unsupportedKind
        }
        let vaultId = try reader.fixedField(2, count: 16)
        let generation = try reader.fixedField(3, count: 16)
        let epoch = try reader.uintField(4)
        guard epoch > 0 else { throw VaultRecordError.invalidEpoch }
        let objectId = try reader.fixedField(5, count: 16)
        let authorId = try reader.fixedField(6, count: 16)
        let revisionId = try reader.fixedField(7, count: 16)
        let membershipHash = try reader.fixedField(8, count: 32)
        let payload = try reader.bytesField(9, limit: maxPayloadBytes)
        let signature = try reader.fixedField(10, count: 64)
        guard reader.isAtEnd else { throw VaultRecordError.malformed }
        return Self(
            binding: VaultRecordBinding(kind: kind, vaultId: vaultId, generation: generation, epoch: epoch,
                                        objectId: objectId, authorId: authorId, membershipHash: membershipHash),
            revisionId: revisionId, payload: payload, signature: signature, maxPayloadBytes: maxPayloadBytes
        )
    }

    func verify(expected: VaultRecordBinding, trustedPublicKey: Data) throws -> VaultVerifiedRecord {
        guard binding == expected else { throw VaultRecordError.contextMismatch }
        let input = try VaultRecordCodec.signingBytes(
            binding: binding, revisionId: revisionId, payload: payload, maxPayloadBytes: maxPayloadBytes
        )
        do {
            try VaultCrypto.verifyEd25519(publicKey: trustedPublicKey, message: input, signature: signature)
        } catch {
            throw VaultRecordError.invalidSignature
        }
        return VaultVerifiedRecord(binding: binding, revisionId: revisionId, payload: payload)
    }
}

struct VaultVerifiedRecord: CustomStringConvertible, CustomDebugStringConvertible {
    let binding: VaultRecordBinding
    let revisionId: Data
    let payload: Data

    fileprivate init(binding: VaultRecordBinding, revisionId: Data, payload: Data) {
        self.binding = binding
        self.revisionId = revisionId
        self.payload = payload
    }

    var description: String { "VerifiedRecord([REDACTED])" }
    var debugDescription: String { description }
}

enum VaultRecordCodec {
    private static let domain = Data("zeron/signed-record/v1\0".utf8)
    private static let maxOverhead = 256

    static func signingBytes(
        binding: VaultRecordBinding, revisionId: Data, payload: Data, maxPayloadBytes: Int
    ) throws -> Data {
        var out = try buffer(binding: binding, revisionId: revisionId, payload: payload, limit: maxPayloadBytes)
        out.append(domain)
        fields(into: &out, count: 10, binding: binding, revisionId: revisionId, payload: payload)
        return out
    }

    static func encodeSigned(
        binding: VaultRecordBinding, revisionId: Data, payload: Data, signature: Data, maxPayloadBytes: Int
    ) throws -> Data {
        guard signature.count == 64 else { throw VaultRecordError.malformed }
        var out = try buffer(binding: binding, revisionId: revisionId, payload: payload, limit: maxPayloadBytes)
        fields(into: &out, count: 11, binding: binding, revisionId: revisionId, payload: payload)
        bytesField(into: &out, key: 10, value: signature)
        return out
    }

    fileprivate static func totalLimit(_ payloadLimit: Int) throws -> Int {
        guard payloadLimit >= 0, payloadLimit <= Int.max - maxOverhead else { throw VaultRecordError.sizeLimitExceeded }
        return payloadLimit + maxOverhead
    }

    private static func buffer(binding: VaultRecordBinding, revisionId: Data, payload: Data, limit: Int) throws -> Data {
        _ = try totalLimit(limit)
        guard binding.epoch > 0 else { throw VaultRecordError.invalidEpoch }
        guard payload.count <= limit else { throw VaultRecordError.sizeLimitExceeded }
        guard [binding.vaultId, binding.generation, binding.objectId, binding.authorId, revisionId].allSatisfy({ $0.count == 16 }),
              binding.membershipHash.count == 32 else { throw VaultRecordError.malformed }
        return Data(capacity: payload.count + maxOverhead)
    }

    static func contextBytes(binding: VaultRecordBinding, revisionId: Data) throws -> Data {
        var output = try buffer(binding: binding, revisionId: revisionId, payload: Data(), limit: 0)
        headerFields(into: &output, count: 9, binding: binding, revisionId: revisionId)
        return output
    }

    private static func fields(into out: inout Data, count: UInt64, binding: VaultRecordBinding, revisionId: Data, payload: Data) {
        headerFields(into: &out, count: count, binding: binding, revisionId: revisionId)
        bytesField(into: &out, key: 9, value: payload)
    }

    private static func headerFields(into out: inout Data, count: UInt64, binding: VaultRecordBinding, revisionId: Data) {
        argument(into: &out, major: 5, value: count)
        uintField(into: &out, key: 0, value: 1)
        uintField(into: &out, key: 1, value: binding.kind.rawValue)
        bytesField(into: &out, key: 2, value: binding.vaultId)
        bytesField(into: &out, key: 3, value: binding.generation)
        uintField(into: &out, key: 4, value: binding.epoch)
        bytesField(into: &out, key: 5, value: binding.objectId)
        bytesField(into: &out, key: 6, value: binding.authorId)
        bytesField(into: &out, key: 7, value: revisionId)
        bytesField(into: &out, key: 8, value: binding.membershipHash)
    }

    static func uintField(into out: inout Data, key: UInt64, value: UInt64) {
        argument(into: &out, major: 0, value: key)
        argument(into: &out, major: 0, value: value)
    }

    static func bytesField(into out: inout Data, key: UInt64, value: Data) {
        argument(into: &out, major: 0, value: key)
        argument(into: &out, major: 2, value: UInt64(value.count))
        out.append(value)
    }

    static func argument(into out: inout Data, major: UInt8, value: UInt64) {
        if value < 24 {
            out.append((major << 5) | UInt8(value))
            return
        }
        let width: Int
        let additional: UInt8
        switch value {
        case 24...0xff: (width, additional) = (1, 24)
        case 0x100...0xffff: (width, additional) = (2, 25)
        case 0x10000...0xffffffff: (width, additional) = (4, 26)
        default: (width, additional) = (8, 27)
        }
        out.append((major << 5) | additional)
        var bigEndian = value.bigEndian
        withUnsafeBytes(of: &bigEndian) { out.append(contentsOf: $0.suffix(width)) }
    }
}

struct VaultRecordReader {
    private let data: Data
    private var offset: Int

    init(_ data: Data) {
        self.data = data
        self.offset = data.startIndex
    }

    var isAtEnd: Bool { offset == data.endIndex }

    private mutating func take(_ count: Int) throws -> Data {
        guard count >= 0, count <= data.endIndex - offset else { throw VaultRecordError.malformed }
        let start = offset
        offset += count
        return data[start..<offset]
    }

    mutating func argument(major: UInt8) throws -> UInt64 {
        let first = try take(1)
        let head = first[first.startIndex]
        guard head >> 5 == major else { throw VaultRecordError.malformed }
        let length: Int
        let minimum: UInt64
        switch head & 31 {
        case 0...23: return UInt64(head & 31)
        case 24: (length, minimum) = (1, 24)
        case 25: (length, minimum) = (2, 0x100)
        case 26: (length, minimum) = (4, 0x10000)
        case 27: (length, minimum) = (8, 0x100000000)
        default: throw VaultRecordError.malformed
        }
        let value = try take(length).reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        guard value >= minimum else { throw VaultRecordError.nonCanonical }
        return value
    }

    private mutating func key(_ expected: UInt64) throws {
        guard try argument(major: 0) == expected else { throw VaultRecordError.malformed }
    }

    mutating func uintField(_ key: UInt64) throws -> UInt64 {
        try self.key(key)
        return try argument(major: 0)
    }

    mutating func bytesField(_ key: UInt64, limit: Int) throws -> Data {
        try self.key(key)
        let length = try argument(major: 2)
        guard length <= UInt64(limit) else { throw VaultRecordError.sizeLimitExceeded }
        return try take(Int(length))
    }

    mutating func fixedField(_ key: UInt64, count: Int) throws -> Data {
        let value = try bytesField(key, limit: count)
        guard value.count == count else { throw VaultRecordError.malformed }
        return value
    }
}
