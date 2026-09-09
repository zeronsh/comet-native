// Registry merge core + client-side doc — the Swift mirror of
// edge/src/registry-core.ts (pure op/row semantics) and
// crates/doc/src/registry.rs (RegistryDoc: authoritative rows + pending
// overlay). See docs/registry-sync.md.
//
// The registry stores CURRENT STATE ONLY: a row is a bag of fields, each
// field carries the HLC of its last write, and a write applies iff its clock
// beats the stored one. The shared conformance vectors live in
// ZeronTests/RegistryCoreTests.swift — keep all three languages in sync.

import Foundation

// MARK: - JSON field values

/// A JSON value that distinguishes explicit `null` from an absent key: op
/// `set` maps use `null` to DELETE a field (still a clocked write), so the
/// JSON layer must carry null through encode/decode losslessly.
enum JSONValue: Hashable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])
}

extension JSONValue: Codable {
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? c.decode(Int64.self) {
            self = .int(i)
        } else if let d = try? c.decode(Double.self) {
            self = .double(d)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else if let o = try? c.decode([String: JSONValue].self) {
            self = .object(o)
        } else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "not a JSON value")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let v): try c.encode(v)
        case .int(let v): try c.encode(v)
        case .double(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        }
    }
}

extension JSONValue {
    var stringValue: String? {
        if case .string(let v) = self { return v }
        return nil
    }

    var boolValue: Bool? {
        if case .bool(let v) = self { return v }
        return nil
    }

    var int64Value: Int64? {
        switch self {
        case .int(let v): return v
        case .double(let v): return Int64(v)
        default: return nil
        }
    }

    var objectValue: [String: JSONValue]? {
        if case .object(let v) = self { return v }
        return nil
    }

    var arrayValue: [JSONValue]? {
        if case .array(let v) = self { return v }
        return nil
    }

    var isNull: Bool { self == .null }

    /// Bridge an Encodable (e.g. ChatConfig) into a field value.
    init?<T: Encodable>(encodable: T) {
        guard let data = try? JSONEncoder().encode(encodable),
              let value = try? JSONDecoder().decode(JSONValue.self, from: data) else { return nil }
        self = value
    }
}

// MARK: - HLC

/// Hybrid-logical-clock string: `{ms:013}-{counter:06}-{device}`. Fixed-width
/// zero padding makes lexicographic order = (ms, counter, device) order, and
/// the device suffix makes it total — two distinct writers can never tie.
typealias Hlc = String

func encodeHlc(ms: Int64, counter: UInt32, device: String) -> Hlc {
    func pad(_ n: some BinaryInteger, _ width: Int) -> String {
        let s = String(n)
        return s.count >= width ? s : String(repeating: "0", count: width - s.count) + s
    }
    return "\(pad(ms, 13))-\(pad(counter, 6))-\(device)"
}

/// `a` strictly newer than `b` (nil = never written, loses to any).
func hlcNewer(_ a: Hlc, _ b: Hlc?) -> Bool {
    guard let b else { return true }
    return a > b
}

/// Monotonic HLC source: never emits the same or an earlier clock twice, even
/// across a wall-clock regression or restart (state persists with the doc).
struct HlcClock: Codable, Hashable, Sendable {
    var lastMs: Int64 = 0
    var counter: UInt32 = 0

    mutating func next(nowMs: Int64, device: String) -> Hlc {
        if nowMs > lastMs {
            lastMs = nowMs
            counter = 0
        } else {
            counter += 1
            if counter > 999_999 {
                lastMs += 1
                counter = 0
            }
        }
        return encodeHlc(ms: lastMs, counter: counter, device: device)
    }
}

// MARK: - Rows and ops (wire-compatible with edge/src/registry-core.ts)

struct RegistryRow: Hashable, Codable, Sendable {
    var kind: String
    var id: String
    /// Server seq of the batch that last touched this row (0 locally).
    var seq: UInt64
    var deleted: Bool
    /// Tombstone clock — an upsert newer than this revives the row.
    var delHlc: Hlc?
    var fields: [String: JSONValue]
    /// Per-field last-write clocks.
    var clocks: [String: Hlc]
}

enum RegistryOpType: String, Codable, Sendable {
    /// Creates, and revives tombstones when newer.
    case upsert
    /// Never creates or revives ("never invent rows").
    case update
    /// Tombstones when causally newer than the row.
    case delete
}

struct RegistryOp: Hashable, Codable, Sendable {
    var kind: String
    var id: String
    var op: RegistryOpType
    /// Field writes; `.null` deletes the field (still a clocked write).
    /// Absent for deletes.
    var set: [String: JSONValue]?
    /// Clock for every write in `set` without an entry in `clocks`.
    var hlc: Hlc
    /// Per-field clock overrides — re-seed pushes carry a row's ORIGINAL
    /// clocks so recovery never coarsens causality.
    var clocks: [String: Hlc]?
}

// MARK: - Validation (structural, mirrors validateOp in registry-core.ts)

/// Per-op serialized budget — a row is an index entry, never a document.
let registryMaxOpBytes = 16 * 1024

private let idRe = try! NSRegularExpression(pattern: "^[A-Za-z0-9_.:@/-]{1,256}$")
private let kindRe = try! NSRegularExpression(pattern: "^[a-z][a-zA-Z0-9]{0,31}$")
private let fieldRe = try! NSRegularExpression(pattern: "^[a-zA-Z][a-zA-Z0-9]{0,63}$")
private let hlcRe = try! NSRegularExpression(pattern: "^\\d{13}-\\d{6}-[A-Za-z0-9_-]{1,128}$")

private func matches(_ re: NSRegularExpression, _ s: String) -> Bool {
    let range = NSRange(s.startIndex..., in: s)
    return re.firstMatch(in: s, range: range) != nil
}

/// Structural validation for an op. Returns an error string or nil. The
/// server re-validates and rejects whole batches — a client that builds one
/// bad op is a client bug to surface, not to ship.
func validateOp(_ op: RegistryOp) -> String? {
    if !matches(kindRe, op.kind) { return "bad kind" }
    if !matches(idRe, op.id) { return "bad id" }
    if !matches(hlcRe, op.hlc) { return "bad hlc" }
    if op.op == .delete {
        if op.set != nil { return "delete carries set" }
    } else {
        guard let set = op.set else { return "missing set" }
        for key in set.keys where !matches(fieldRe, key) {
            return "bad field: \(key)"
        }
    }
    if let clocks = op.clocks {
        for (key, hlc) in clocks {
            if !matches(fieldRe, key) { return "bad clock field: \(key)" }
            if !matches(hlcRe, hlc) { return "bad clock: \(key)" }
        }
    }
    if let data = try? JSONEncoder().encode(op), data.count > registryMaxOpBytes {
        return "op too large"
    }
    return nil
}

// MARK: - Merge (mirrors applyOp in registry-core.ts exactly)

struct RegistryApplyResult {
    /// nil only for an `update` op on a missing row (nothing to store).
    var row: RegistryRow?
    var changed: Bool
}

/// Apply one op to a row (absent = nil). Pure; returns the (possibly new) row
/// and whether anything changed. Callers stamp `row.seq` on change.
///
/// Rules (docs/registry-sync.md):
/// - field write applies iff clock > stored clock for that field;
/// - `update` never creates or revives a row ("never invent rows");
/// - `upsert` creates, and revives a tombstone iff newer than `delHlc`;
/// - `delete` tombstones iff newer than `delHlc`, clearing fields/clocks;
/// - re-applying any op is a no-op (strict `>`), so re-pushes are idempotent.
func applyOp(_ row: RegistryRow?, _ op: RegistryOp) -> RegistryApplyResult {
    func clockFor(_ field: String) -> Hlc { op.clocks?[field] ?? op.hlc }

    if op.op == .delete {
        guard var gone = row else {
            // Tombstone-on-missing guards against a late create racing the delete.
            return RegistryApplyResult(
                row: RegistryRow(kind: op.kind, id: op.id, seq: 0, deleted: true,
                                 delHlc: op.hlc, fields: [:], clocks: [:]),
                changed: true)
        }
        let beats = gone.deleted ? hlcNewer(op.hlc, gone.delHlc) : hlcNewer(op.hlc, maxClock(gone))
        guard beats else { return RegistryApplyResult(row: row, changed: false) }
        gone.deleted = true
        gone.delHlc = op.hlc
        gone.fields = [:]
        gone.clocks = [:]
        return RegistryApplyResult(row: gone, changed: true)
    }

    var base: RegistryRow
    if let row {
        if row.deleted {
            if op.op == .update { return RegistryApplyResult(row: row, changed: false) }
            if !hlcNewer(op.hlc, row.delHlc) { return RegistryApplyResult(row: row, changed: false) }
            // Revival: the tombstone loses wholesale; the upsert's fields are the row.
            base = RegistryRow(kind: row.kind, id: row.id, seq: row.seq, deleted: false,
                               delHlc: row.delHlc, fields: [:], clocks: [:])
        } else {
            base = row
        }
    } else {
        if op.op == .update { return RegistryApplyResult(row: nil, changed: false) }
        base = RegistryRow(kind: op.kind, id: op.id, seq: 0, deleted: false,
                           delHlc: nil, fields: [:], clocks: [:])
    }

    var changed = row == nil || (row?.deleted == true && !base.deleted)
    for (key, value) in op.set ?? [:] {
        let clock = clockFor(key)
        guard hlcNewer(clock, base.clocks[key]) else { continue }
        if value.isNull {
            base.fields.removeValue(forKey: key)
        } else {
            base.fields[key] = value
        }
        base.clocks[key] = clock
        changed = true
    }
    guard changed else { return RegistryApplyResult(row: base, changed: false) }
    base.deleted = false
    return RegistryApplyResult(row: base, changed: true)
}

/// The newest clock anywhere on a live row (delete-vs-live comparison base):
/// a delete only wins over a row it is causally newer than.
func maxClock(_ row: RegistryRow) -> Hlc? {
    var max = row.delHlc
    for clock in row.clocks.values where max == nil || clock > max! {
        max = clock
    }
    return max
}

/// A row as a re-seed op (server-behind-client recovery): one upsert carrying
/// the row's ORIGINAL per-field clocks, or a delete for tombstones.
func rowToSeedOp(_ row: RegistryRow) -> RegistryOp {
    if row.deleted {
        return RegistryOp(kind: row.kind, id: row.id, op: .delete, set: nil,
                          hlc: row.delHlc ?? encodeHlc(ms: 0, counter: 0, device: "seed"),
                          clocks: nil)
    }
    return RegistryOp(kind: row.kind, id: row.id, op: .upsert, set: row.fields,
                      hlc: maxClock(row) ?? encodeHlc(ms: 0, counter: 0, device: "seed"),
                      clocks: row.clocks)
}

// MARK: - Pending batches

struct RegistryPendingBatch: Hashable, Codable, Sendable {
    var batch: String
    var ops: [RegistryOp]
    /// True while the batch is in flight on the CURRENT connection (cleared
    /// on disconnect so reconnects re-push). Never persisted — a restart
    /// implies a fresh connection.
    var inFlight = false

    private enum CodingKeys: String, CodingKey {
        case batch, ops
    }
}

// MARK: - The doc (mirrors crates/doc/src/registry.rs RegistryDoc)

/// What `applyState` decided about a `state` frame.
enum RegistryStateOutcome: Equatable {
    /// Delta applied over existing authoritative rows.
    case delta
    /// Full state replaced local authoritative rows.
    case replaced
    /// The server is BEHIND this client (wiped/reset storage): local rows were
    /// kept and re-seed batches were enqueued. The caller must push pending.
    case reseeded
}

/// The local registry replica: authoritative rows (server truth, replaced
/// wholesale by state/rows frames — clients never re-merge server rows) plus
/// a pending op-batch queue (local writes not yet acked, replayed over the
/// authoritative rows for every read). Pure data, no I/O — WorkspaceStore
/// drives it on the main actor; unit tests drive it directly.
final class RegistryDoc {
    /// Ops per push batch cap is 500 server-side; seeds chunk under it.
    static let seedChunkOps = 400

    let deviceId: String
    /// kind → id → row (server truth).
    private(set) var authoritative: [String: [String: RegistryRow]] = [:]
    private(set) var serverSeq: UInt64 = 0
    private(set) var gcFloor: UInt64 = 0
    private(set) var clock = HlcClock()
    private(set) var pending: [RegistryPendingBatch] = []
    /// Bumped on every mutation (local or applied) — feeds save debounces.
    private(set) var generation: UInt64 = 0

    init(deviceId: String) {
        self.deviceId = deviceId
    }

    /// Sync cursor: the last server seq this replica has fully applied.
    var cursor: UInt64 { serverSeq }

    /// The cursor for a hello frame: nil (ask for full state) only when this
    /// replica has never seen or written anything.
    var helloCursor: UInt64? {
        (serverSeq == 0 && pending.isEmpty && generation == 0) ? nil : serverSeq
    }

    // MARK: Persistence ({rows, cursor, gcFloor, clock, pending} JSON blob)

    /// Bump to force one full resync on next load (heals snapshots whose
    /// cursor jumped past unapplied rows; mirrors the desktop epoch).
    private static let currentResyncEpoch = 1

    private struct Persisted: Codable {
        var v: Int
        var resyncEpoch: Int?
        var deviceId: String
        var serverSeq: UInt64
        var gcFloor: UInt64
        var clock: HlcClock
        var rows: [RegistryRow]
        var pending: [RegistryPendingBatch]
    }

    func toData() throws -> Data {
        let state = Persisted(v: 1, resyncEpoch: Self.currentResyncEpoch,
                              deviceId: deviceId, serverSeq: serverSeq, gcFloor: gcFloor,
                              clock: clock, rows: authoritative.values.flatMap(\.values),
                              pending: pending)
        return try JSONEncoder().encode(state)
    }

    static func from(data: Data, deviceId: String) throws -> RegistryDoc {
        let state = try JSONDecoder().decode(Persisted.self, from: data)
        guard state.v == 1 else {
            throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: "unknown registry snapshot version \(state.v)"))
        }
        let doc = RegistryDoc(deviceId: deviceId)
        // Pre-epoch snapshots may hide a jumped cursor: zero it once so the
        // next hello returns full state. Rows are kept.
        doc.serverSeq = (state.resyncEpoch ?? 0) < Self.currentResyncEpoch ? 0 : state.serverSeq
        doc.gcFloor = state.gcFloor
        doc.clock = state.clock
        doc.pending = state.pending
        for row in state.rows {
            doc.putAuthoritative(row)
        }
        return doc
    }

    // MARK: Server frames

    /// Apply a hello `state` frame. See `RegistryStateOutcome` for the shapes.
    @discardableResult
    func applyState(seq: UInt64, full: Bool, gcFloor: UInt64, rows: [RegistryRow]) -> RegistryStateOutcome {
        generation += 1
        if !full {
            for row in rows { putAuthoritative(row) }
            serverSeq = seq
            self.gcFloor = gcFloor
            return .delta
        }
        if seq < serverSeq {
            // The server lost state (reset/wipe). Local rows are the only
            // copy: keep them and re-seed the server with ORIGINAL clocks.
            let seed = authoritative.values.flatMap(\.values).map(rowToSeedOp)
            for row in rows { putAuthoritative(row) }
            serverSeq = seq
            self.gcFloor = gcFloor
            enqueueSeed(seed)
            return .reseeded
        }
        // Full replace — but local-only rows the server never saw (e.g. it
        // GC-jumped our cursor while we held unseeded rows) re-seed too.
        var incoming: [String: [String: RegistryRow]] = [:]
        for row in rows {
            incoming[row.kind, default: [:]][row.id] = row
        }
        var seed: [RegistryOp] = []
        for (kind, byId) in authoritative {
            for (id, row) in byId where incoming[kind]?[id] == nil {
                seed.append(rowToSeedOp(row))
            }
        }
        authoritative = incoming
        serverSeq = seq
        self.gcFloor = gcFloor
        guard !seed.isEmpty else { return .replaced }
        enqueueSeed(seed)
        return .reseeded
    }

    /// Apply a `rows` broadcast (merged truth for the touched rows).
    /// Returns false on a SEQ GAP — frames between the cursor and this batch
    /// were missed: the rows still apply, but the cursor holds so the caller
    /// resyncs (a fresh hello backfills the window). Advancing across a gap
    /// makes the missed rows permanently invisible (desktop field incident).
    func applyRows(seq: UInt64, rows: [RegistryRow]) -> Bool {
        generation += 1
        for row in rows { putAuthoritative(row) }
        if seq > serverSeq + 1 { return false }
        if seq > serverSeq { serverSeq = seq }
        return true
    }

    /// Retire an acked batch. Deliberately does NOT advance the cursor: the
    /// ack's seq is OUR batch's position, and peers' batches may sit between
    /// the cursor and it — jumping skipped them forever (desktop field
    /// incident; the cursor advances only with actual row payloads).
    func ackBatch(_ batch: String, seq: UInt64) {
        let before = pending.count
        pending.removeAll { $0.batch == batch }
        if pending.count != before { generation += 1 }
    }

    /// Batches to push: everything not already in flight on this connection.
    func takePushable() -> [RegistryPendingBatch] {
        var out: [RegistryPendingBatch] = []
        for ix in pending.indices where !pending[ix].inFlight {
            pending[ix].inFlight = true
            out.append(pending[ix])
        }
        return out
    }

    /// Connection dropped: everything unacked becomes pushable again.
    func markDisconnected() {
        for ix in pending.indices {
            pending[ix].inFlight = false
        }
    }

    private func putAuthoritative(_ row: RegistryRow) {
        authoritative[row.kind, default: [:]][row.id] = row
    }

    // MARK: Local writes

    private func nextHlc() -> Hlc {
        clock.next(nowMs: nowMs(), device: deviceId)
    }

    func enqueue(ops: [RegistryOp]) {
        guard !ops.isEmpty else { return }
        generation += 1
        // Batch ids only need device-lifetime uniqueness; the HLC of a fresh
        // tick provides exactly that.
        pending.append(RegistryPendingBatch(batch: "b-\(nextHlc())", ops: ops))
    }

    /// Seeds can exceed the server's 500-op batch cap on a large workspace —
    /// chunk them (the cascade-delete batches real writes produce never do).
    private func enqueueSeed(_ ops: [RegistryOp]) {
        guard !ops.isEmpty else { return }
        for start in stride(from: 0, to: ops.count, by: Self.seedChunkOps) {
            enqueue(ops: Array(ops[start..<min(start + Self.seedChunkOps, ops.count)]))
        }
    }

    func write(kind: String, id: String, op: RegistryOpType, set: [String: JSONValue]) {
        enqueue(ops: [RegistryOp(kind: kind, id: id, op: op, set: set, hlc: nextHlc(), clocks: nil)])
    }

    /// Tombstone rows in ONE batch (a space delete cascades space + chats +
    /// sessions atomically).
    func deleteRows(_ keys: [(kind: String, id: String)]) {
        let hlc = nextHlc()
        enqueue(ops: keys.map {
            RegistryOp(kind: $0.kind, id: $0.id, op: .delete, set: nil, hlc: hlc, clocks: nil)
        })
    }

    // MARK: Overlay reads (authoritative + pending ops replayed)

    /// The row as this device should display it: authoritative + pending ops.
    func overlayRow(kind: String, id: String) -> RegistryRow? {
        var row = authoritative[kind]?[id]
        for batch in pending {
            for op in batch.ops where op.kind == kind && op.id == id {
                if let next = applyOp(row, op).row {
                    row = next
                }
            }
        }
        guard let row, !row.deleted else { return nil }
        return row
    }

    /// All live rows of `kind`, overlay applied.
    func overlayRows(kind: String) -> [RegistryRow] {
        var ids = authoritative[kind].map { Array($0.keys) } ?? []
        var seen = Set(ids)
        for batch in pending {
            for op in batch.ops where op.kind == kind && !seen.contains(op.id) {
                seen.insert(op.id)
                ids.append(op.id)
            }
        }
        return ids.compactMap { overlayRow(kind: kind, id: $0) }
    }

    func rowExists(kind: String, id: String) -> Bool {
        overlayRow(kind: kind, id: id) != nil
    }
}
