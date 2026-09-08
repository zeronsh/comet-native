import CryptoKit
import Foundation
import Security
import Darwin

enum VaultStorageError: Error, Equatable {
    case keychain(OSStatus)
    case keyUnavailable
    case invalidState
    case tooLarge
    case io(Int32)
}

protocol VaultSecretStorage: Sendable {
    func load(account: String) throws -> Data?
    func insert(account: String, value: Data) throws
}

struct VaultKeychainStorage: VaultSecretStorage {
    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: "sh.zeron.ios.vault",
         kSecAttrAccount as String: account,
         kSecAttrSynchronizable as String: false]
    }

    func load(account: String) throws -> Data? {
        var query = query(account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw VaultStorageError.keychain(status) }
        guard let data = result as? Data, data.count == 32 else { throw VaultStorageError.invalidState }
        return data
    }

    func insert(account: String, value: Data) throws {
        guard value.count == 32 else { throw VaultStorageError.invalidState }
        var query = query(account)
        query[kSecValueData as String] = value
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess || status == errSecDuplicateItem else {
            throw VaultStorageError.keychain(status)
        }
    }
}

struct VaultPersistence: Sendable {
    static let maxBytes = 8 * 1024 * 1024
    let directory: URL
    let account: String
    let secrets: any VaultSecretStorage

    init(origin: URL, orgId: String, userId: String, directory: URL? = nil,
         secrets: any VaultSecretStorage = VaultKeychainStorage()) {
        account = Data(SHA256.hash(data: Data("zeron/ios/store/v1\0\(origin.absoluteString)\0\(orgId)\0\(userId)".utf8))).vaultHex
        self.directory = directory ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("ZeronVault", isDirectory: true).appendingPathComponent(account, isDirectory: true)
        self.secrets = secrets
    }

    var stateURL: URL { directory.appendingPathComponent("state.enc") }
    var exists: Bool {
        var info = stat()
        return lstat(stateURL.path, &info) == 0 || errno != ENOENT
    }
    private var aad: Data { Data("zeron/ios/vault-state/v1\0\(account)".utf8) }

    func load() throws -> Data? {
        guard exists else { return nil }
        guard let key = try secrets.load(account: account), key.count == 32 else { throw VaultStorageError.keyUnavailable }
        let values = try stateURL.resourceValues(forKeys: [.fileSizeKey, .isSymbolicLinkKey])
        guard values.isSymbolicLink != true else { throw VaultStorageError.invalidState }
        guard let size = values.fileSize, size <= Self.maxBytes else { throw VaultStorageError.tooLarge }
        let encoded = try Data(contentsOf: stateURL)
        guard encoded.first == 1 else { throw VaultStorageError.invalidState }
        return try AES.GCM.open(AES.GCM.SealedBox(combined: encoded.dropFirst()),
                                using: SymmetricKey(data: key), authenticating: aad)
    }

    func save(_ data: Data) throws {
        guard data.count <= Self.maxBytes - 64 else { throw VaultStorageError.tooLarge }
        if try secrets.load(account: account) == nil {
            guard !exists else { throw VaultStorageError.keyUnavailable }
            try secrets.insert(account: account, value: SymmetricKey(size: .bits256).withUnsafeBytes { Data($0) })
        }
        guard let key = try secrets.load(account: account), key.count == 32 else { throw VaultStorageError.keyUnavailable }
        let box = try AES.GCM.seal(data, using: SymmetricKey(data: key), authenticating: aad)
        guard let combined = box.combined else { throw VaultStorageError.invalidState }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var location = directory
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try location.setResourceValues(values)
        try Self.writeDurably(Data([1]) + combined, to: stateURL)
    }

    static func writeDurably(_ data: Data, to url: URL) throws {
        var options: Data.WritingOptions = [.atomic]
        #if os(iOS)
        options.insert(.completeFileProtectionUntilFirstUserAuthentication)
        #endif
        try data.write(to: url, options: options)
        let file = try FileHandle(forWritingTo: url)
        defer { try? file.close() }
        try file.synchronize()
        let descriptor = Darwin.open(url.deletingLastPathComponent().path, O_RDONLY | O_CLOEXEC)
        guard descriptor >= 0 else { throw VaultStorageError.io(errno) }
        defer { Darwin.close(descriptor) }
        guard fsync(descriptor) == 0 else { throw VaultStorageError.io(errno) }
    }
}

extension Data {
    var vaultHex: String { map { String(format: "%02x", $0) }.joined() }

    init?(vaultHex: String, count: Int) {
        let bytes = Array(vaultHex.utf8)
        guard bytes.count == count * 2, bytes.allSatisfy({ $0 < 128 }) else { return nil }
        var output = Data(capacity: count)
        for index in stride(from: 0, to: bytes.count, by: 2) {
            guard let value = UInt8(String(decoding: bytes[index..<(index + 2)], as: UTF8.self), radix: 16) else { return nil }
            output.append(value)
        }
        self = output
    }
}
