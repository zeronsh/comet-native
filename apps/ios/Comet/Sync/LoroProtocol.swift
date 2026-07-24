// loro-protocol 0.3 wire codec — a Swift port of the loro-protocol crate's
// encoding.rs, byte-compatible with the Rust engine and the TS edge.
//
// Frame layout: [4-byte CRDT magic][varBytes room_id][1-byte type][payload].
// Varints are ULEB128; varBytes/varString are length-prefixed; batch ids are
// 8 raw bytes (fixed width for hot-path parsing). Max message 256 KiB.

import Foundation

enum CrdtType: Equatable {
    case loro
    case loroEphemeral

    var magic: [UInt8] {
        switch self {
        case .loro: return Array("%LOR".utf8)
        case .loroEphemeral: return Array("%EPH".utf8)
        }
    }

    static func from(magic: ArraySlice<UInt8>) -> CrdtType? {
        let bytes = Array(magic)
        if bytes == Array("%LOR".utf8) { return .loro }
        if bytes == Array("%EPH".utf8) { return .loroEphemeral }
        return nil
    }
}

struct BatchId: Equatable, Hashable {
    var bytes: [UInt8]  // exactly 8

    static func random() -> BatchId {
        BatchId(bytes: (0..<8).map { _ in UInt8.random(in: 0...255) })
    }
}

enum JoinErrorCode: UInt8 {
    case unknown = 0x00
    case versionUnknown = 0x01
    case authFailed = 0x02
    case appError = 0x7f
}

enum UpdateStatusCode: UInt8 {
    case ok = 0x00
    case unknown = 0x01
    case permissionDenied = 0x03
    case invalidUpdate = 0x04
    case payloadTooLarge = 0x05
    case rateLimited = 0x06
    case fragmentTimeout = 0x07
    case appError = 0x7f
}

enum RoomErrorCode: UInt8 {
    case rejoinSuggested = 0x01
    case evicted = 0x02
    case unknown = 0x7f
}

enum ProtocolMessage {
    case joinRequest(crdt: CrdtType, roomId: String, auth: [UInt8], version: [UInt8])
    case joinResponseOk(crdt: CrdtType, roomId: String, permission: String, version: [UInt8], extra: [UInt8])
    case joinError(crdt: CrdtType, roomId: String, code: JoinErrorCode, message: String)
    case docUpdate(crdt: CrdtType, roomId: String, updates: [[UInt8]], batchId: BatchId)
    case docUpdateFragmentHeader(crdt: CrdtType, roomId: String, batchId: BatchId, fragmentCount: UInt64, totalSizeBytes: UInt64)
    case docUpdateFragment(crdt: CrdtType, roomId: String, batchId: BatchId, index: UInt64, fragment: [UInt8])
    case roomError(crdt: CrdtType, roomId: String, code: RoomErrorCode, message: String)
    case ack(crdt: CrdtType, roomId: String, refId: BatchId, status: UpdateStatusCode)
    case leave(crdt: CrdtType, roomId: String)

    var typeByte: UInt8 {
        switch self {
        case .joinRequest: return 0x00
        case .joinResponseOk: return 0x01
        case .joinError: return 0x02
        case .docUpdate: return 0x03
        case .docUpdateFragmentHeader: return 0x04
        case .docUpdateFragment: return 0x05
        case .roomError: return 0x06
        case .ack: return 0x08
        case .leave: return 0x07
        }
    }
}

enum LoroWire {
    static let maxMessageSize = 256 * 1024
    static let fragmentBytes = 200_000

    // MARK: Encode

    static func encode(_ message: ProtocolMessage) -> Data? {
        var w = ByteWriter()
        switch message {
        case .joinRequest(let crdt, let roomId, let auth, let version):
            header(&w, crdt, roomId, message.typeByte)
            w.varBytes(auth)
            w.varBytes(version)
        case .joinResponseOk(let crdt, let roomId, let permission, let version, let extra):
            header(&w, crdt, roomId, message.typeByte)
            w.varString(permission)
            w.varBytes(version)
            w.varBytes(extra)
        case .joinError(let crdt, let roomId, let code, let msg):
            header(&w, crdt, roomId, message.typeByte)
            w.byte(code.rawValue)
            w.varString(msg)
        case .docUpdate(let crdt, let roomId, let updates, let batchId):
            header(&w, crdt, roomId, message.typeByte)
            w.uleb128(UInt64(updates.count))
            for u in updates { w.varBytes(u) }
            w.raw(batchId.bytes)
        case .docUpdateFragmentHeader(let crdt, let roomId, let batchId, let count, let total):
            header(&w, crdt, roomId, message.typeByte)
            w.raw(batchId.bytes)
            w.uleb128(count)
            w.uleb128(total)
        case .docUpdateFragment(let crdt, let roomId, let batchId, let index, let fragment):
            header(&w, crdt, roomId, message.typeByte)
            w.raw(batchId.bytes)
            w.uleb128(index)
            w.varBytes(fragment)
        case .roomError(let crdt, let roomId, let code, let msg):
            header(&w, crdt, roomId, message.typeByte)
            w.byte(code.rawValue)
            w.varString(msg)
        case .ack(let crdt, let roomId, let refId, let status):
            header(&w, crdt, roomId, message.typeByte)
            w.raw(refId.bytes)
            w.byte(status.rawValue)
        case .leave(let crdt, let roomId):
            header(&w, crdt, roomId, message.typeByte)
        }
        guard w.data.count <= maxMessageSize else { return nil }
        return Data(w.data)
    }

    private static func header(_ w: inout ByteWriter, _ crdt: CrdtType, _ roomId: String, _ type: UInt8) {
        w.raw(crdt.magic)
        w.varBytes(Array(roomId.utf8))
        w.byte(type)
    }

    // MARK: Decode

    static func decode(_ data: Data) -> ProtocolMessage? {
        var r = ByteReader(bytes: [UInt8](data))
        guard let magic = r.read(4), let crdt = CrdtType.from(magic: magic[...]),
              let roomBytes = r.varBytes(), roomBytes.count <= 128,
              let roomId = String(bytes: roomBytes, encoding: .utf8),
              let type = r.readByte() else { return nil }

        switch type {
        case 0x00:
            guard let auth = r.varBytes(), let version = r.varBytes() else { return nil }
            return .joinRequest(crdt: crdt, roomId: roomId, auth: auth, version: version)
        case 0x01:
            guard let perm = r.varString(), let version = r.varBytes() else { return nil }
            let extra = r.varBytes() ?? []
            return .joinResponseOk(crdt: crdt, roomId: roomId, permission: perm, version: version, extra: extra)
        case 0x02:
            guard let codeByte = r.readByte(), let msg = r.varString() else { return nil }
            let code = JoinErrorCode(rawValue: codeByte) ?? .unknown
            return .joinError(crdt: crdt, roomId: roomId, code: code, message: msg)
        case 0x03:
            guard let count = r.uleb128() else { return nil }
            var updates: [[UInt8]] = []
            for _ in 0..<count {
                guard let u = r.varBytes() else { return nil }
                updates.append(u)
            }
            guard let id = r.read(8) else { return nil }
            return .docUpdate(crdt: crdt, roomId: roomId, updates: updates, batchId: BatchId(bytes: id))
        case 0x04:
            guard let id = r.read(8), let count = r.uleb128(), let total = r.uleb128() else { return nil }
            return .docUpdateFragmentHeader(crdt: crdt, roomId: roomId, batchId: BatchId(bytes: id),
                                            fragmentCount: count, totalSizeBytes: total)
        case 0x05:
            guard let id = r.read(8), let index = r.uleb128(), let fragment = r.varBytes() else { return nil }
            return .docUpdateFragment(crdt: crdt, roomId: roomId, batchId: BatchId(bytes: id),
                                      index: index, fragment: fragment)
        case 0x06:
            guard let codeByte = r.readByte(), let msg = r.varString() else { return nil }
            let code = RoomErrorCode(rawValue: codeByte) ?? .unknown
            return .roomError(crdt: crdt, roomId: roomId, code: code, message: msg)
        case 0x08:
            guard let id = r.read(8), let statusByte = r.readByte() else { return nil }
            let status = UpdateStatusCode(rawValue: statusByte) ?? .unknown
            return .ack(crdt: crdt, roomId: roomId, refId: BatchId(bytes: id), status: status)
        case 0x07:
            return .leave(crdt: crdt, roomId: roomId)
        default:
            return nil
        }
    }
}

// MARK: - Byte primitives

struct ByteWriter {
    var data: [UInt8] = []

    mutating func byte(_ b: UInt8) { data.append(b) }
    mutating func raw(_ bytes: [UInt8]) { data.append(contentsOf: bytes) }

    mutating func uleb128(_ value: UInt64) {
        var v = value
        repeat {
            var b = UInt8(v & 0x7f)
            v >>= 7
            if v != 0 { b |= 0x80 }
            data.append(b)
        } while v != 0
    }

    mutating func varBytes(_ bytes: [UInt8]) {
        uleb128(UInt64(bytes.count))
        raw(bytes)
    }

    mutating func varString(_ s: String) {
        varBytes(Array(s.utf8))
    }
}

struct ByteReader {
    let bytes: [UInt8]
    var offset = 0

    var remaining: Int { bytes.count - offset }

    mutating func readByte() -> UInt8? {
        guard offset < bytes.count else { return nil }
        defer { offset += 1 }
        return bytes[offset]
    }

    mutating func read(_ count: Int) -> [UInt8]? {
        guard offset + count <= bytes.count else { return nil }
        defer { offset += count }
        return Array(bytes[offset..<offset + count])
    }

    mutating func uleb128() -> UInt64? {
        var result: UInt64 = 0
        var shift: UInt64 = 0
        while true {
            guard let b = readByte() else { return nil }
            result |= UInt64(b & 0x7f) << shift
            if b & 0x80 == 0 { return result }
            shift += 7
            if shift > 63 { return nil }
        }
    }

    mutating func varBytes() -> [UInt8]? {
        guard let len = uleb128(), len <= UInt64(remaining) else { return nil }
        return read(Int(len))
    }

    mutating func varString() -> String? {
        guard let b = varBytes() else { return nil }
        return String(bytes: b, encoding: .utf8)
    }
}
