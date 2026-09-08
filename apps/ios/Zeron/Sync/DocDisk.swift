// On-device Loro doc persistence — the old mobile app's snapshot cache
// (kv.ts/loro-room.ts) and the engine's DocsStore, in file form: one snapshot
// per doc under Application Support. Docs load BEFORE the room join, so the
// UI renders instantly from local state (offline included) and the join's
// version vector turns the backfill incremental instead of a full snapshot.

import Foundation
import Loro

enum DocDisk {
    static var directory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory,
                                            in: .userDomainMask)[0]
            .appendingPathComponent("ZeronDocs", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    static func url(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("\(safe).loro")
    }

    /// Import the saved snapshot, if any. Returns whether anything loaded.
    @discardableResult
    static func load(into doc: LoroDoc, id: String) -> Bool {
        guard let data = try? Data(contentsOf: url(for: id)), !data.isEmpty else { return false }
        return (try? doc.importWith(bytes: data, origin: "disk")) != nil
    }

    /// Atomically persist the doc's snapshot.
    static func save(doc: LoroDoc, id: String) {
        guard let data = try? doc.export(mode: .snapshot) else { return }
        try? data.write(to: url(for: id), options: .atomic)
    }

    /// The workspace registry's persisted blob ({rows, cursor, gcFloor,
    /// clock, pending} JSON — RegistryDoc.toData). Replaces the old `ws3_`
    /// Loro workspace snapshot; session docs stay Loro snapshots unchanged.
    static func registryURL(orgId: String, userId: String) -> URL {
        directory.appendingPathComponent("registry1_\(orgId)_\(userId).json")
    }

    // MARK: chat2 lineage snapshots (docs/chat2-sync.md C2)

    /// `c2_<id>.loro` = 8-byte magic + UInt64 LE room cursor + snapshot,
    /// written atomically in ONE file so doc content and cursor can never
    /// diverge (a restored/copied doc that disagreed with its own cursor was
    /// the root of the s2 redownload-forever class). The un-prefixed
    /// `<id>.loro` files are the retired s2 lineage — never loaded into a
    /// chat2 doc (unrelated Loro histories would duplicate every message),
    /// kept on disk for rollback until LRU pruning ages them out.
    private static let chat2Magic = Data("C2SNAP01".utf8)

    static func chat2URL(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("c2_\(safe).loro")
    }

    static func legacySnapshotExists(id: String) -> Bool {
        FileManager.default.fileExists(atPath: url(for: id).path)
    }

    /// Import the chat2 snapshot; returns its cursor, or nil when absent or
    /// unreadable (caller starts fresh at cursor 0 — the room re-serves).
    static func loadChat2(into doc: LoroDoc, id: String) -> UInt64? {
        guard let data = try? Data(contentsOf: chat2URL(for: id)),
              data.count >= 16, data.prefix(8) == chat2Magic else { return nil }
        var cursor: UInt64 = 0
        for (ix, byte) in data.subdata(in: 8..<16).enumerated() {
            cursor |= UInt64(byte) << (8 * ix)
        }
        guard data.count > 16 else { return cursor }
        guard (try? doc.importWith(bytes: data.subdata(in: 16..<data.count),
                                   origin: "disk")) != nil else { return nil }
        return cursor
    }

    /// Atomically persist the chat2 doc snapshot + its room cursor.
    static func saveChat2(doc: LoroDoc, id: String, cursor: UInt64) {
        try? persistChat2(doc: doc, id: id, cursor: cursor)
    }

    static func persistChat2(doc: LoroDoc, id: String, cursor: UInt64) throws {
        let snapshot = try doc.export(mode: .snapshot)
        var data = chat2Magic
        var le = cursor.littleEndian
        withUnsafeBytes(of: &le) { data.append(contentsOf: $0) }
        data.append(snapshot)
        try VaultPersistence.writeDurably(data, to: chat2URL(for: id))
    }

    /// LRU-prune session snapshots (the workspace registry blob is always
    /// kept; a leftover `ws3_` Loro snapshot is retained for rollback).
    static func prune(keep: Int) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: directory,
                                                      includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        let sessions = files.filter {
            $0.pathExtension == "loro"  // never the registry blob or uploads/
                && !$0.lastPathComponent.hasPrefix("ws3_")
                && !$0.lastPathComponent.hasPrefix("registry1_")
        }
        guard sessions.count > keep else { return }
        let sorted = sessions.sorted {
            let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            return a > b
        }
        for stale in sorted.dropFirst(keep) {
            try? fm.removeItem(at: stale)
        }
    }

    /// Sign-out hygiene: local doc state belongs to the signed-in identity.
    static func wipeAll() {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// Debounced snapshot persistence shared by the doc stores: poke on every
/// change; `save` runs ~1.5s after the last poke, and `flush` forces it
/// (backgrounding, store teardown). The closure captures whatever must be
/// written together (e.g. a chat2 doc AND its cursor — one atomic file).
@MainActor
final class DocSaver {
    private let save: () -> Void
    private var generation = 0
    private var dirty = false

    init(save: @escaping () -> Void) {
        self.save = save
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        save()
    }
}

/// DocSaver's registry twin: debounced persistence for the registry blob.
/// Poke on every mutation; the blob writes ~1.5s after the last poke, and
/// `flush` forces it (backgrounding, store teardown).
@MainActor
final class RegistrySaver {
    private let url: URL
    private let data: () -> Data?
    private var generation = 0
    private var dirty = false

    init(url: URL, data: @escaping () -> Data?) {
        self.url = url
        self.data = data
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        guard let data = data() else { return }
        try? data.write(to: url, options: .atomic)
    }
}
