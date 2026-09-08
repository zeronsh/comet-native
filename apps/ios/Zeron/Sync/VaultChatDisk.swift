import CryptoKit
import Foundation

final class VaultUpdateBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var updates: [Data] = []
    func append(_ bytes: Data) { lock.withLock { updates.append(bytes) } }
    func take() -> [Data] {
        lock.withLock {
            let result = updates
            updates.removeAll()
            return result
        }
    }
}

struct VaultChatBatch: Codable, Equatable, Sendable {
    var id: String
    var bytes: Data
}

struct VaultChatState: Codable, Sendable {
    var version = 1
    var profile: String
    var chatId: String
    var snapshot = Data()
    var cursor: UInt64 = 0
    var unsealed: [Data] = []
    var outbox: [VaultChatBatch] = []
}

struct VaultChatDisk: Sendable {
    static let maxBytes = 64 * 1024 * 1024
    let directory: URL
    let profile: String
    let chatId: String
    var url: URL {
        directory.appendingPathComponent(Data(SHA256.hash(data: Data(chatId.utf8))).vaultHex + ".chat")
    }

    func load() throws -> VaultChatState? {
        guard FileManager.default.fileExists(atPath: url.path) else { return nil }
        guard let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize, size <= Self.maxBytes else { throw VaultStorageError.tooLarge }
        let state = try JSONDecoder().decode(VaultChatState.self, from: Data(contentsOf: url))
        guard state.version == 1, state.profile == profile, state.chatId == chatId,
              Set(state.outbox.map(\.id)).count == state.outbox.count else { throw VaultStorageError.invalidState }
        for batch in state.outbox {
            let record = try VaultUnverifiedRecord.parse(batch.bytes, maxPayloadBytes: 1024 * 1024)
            guard record.untrustedRevisionId.vaultHex == batch.id, record.untrustedBinding.kind == .content else { throw VaultStorageError.invalidState }
        }
        return state
    }

    func save(_ state: VaultChatState) throws {
        guard state.profile == profile, state.chatId == chatId else { throw VaultStorageError.invalidState }
        let data = try JSONEncoder().encode(state)
        guard data.count <= Self.maxBytes else { throw VaultStorageError.tooLarge }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let files = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.fileSizeKey])
        var total = data.count
        for file in files where file.pathExtension == "chat" && file != url {
            total += try file.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
            guard total <= Self.maxBytes else { throw VaultStorageError.tooLarge }
        }
        try VaultPersistence.writeDurably(data, to: url)
    }
}
