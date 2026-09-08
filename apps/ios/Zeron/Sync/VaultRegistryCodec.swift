import Foundation

struct VaultRegistryCodec {
    struct Field: Codable {
        var kind: String
        var id: String
        var field: String
        var hlc: String
        var value: JSONValue
    }

    let vault: MobileVault
    let client: MobileVaultClient
    let userId: String
    private var object: Data { MobileVault.objectId(kind: "registry", id: userId) }

    func seal(_ batch: RegistryPendingBatch) async throws -> RegistryPendingBatch {
        var sealed = batch
        for index in sealed.ops.indices {
            let op = sealed.ops[index]
            guard let fields = op.set else { continue }
            var output: [String: JSONValue] = [:]
            for (name, value) in fields {
                let clock = op.clocks?[name] ?? op.hlc
                let data = try JSONEncoder().encode(Field(kind: op.kind, id: op.id, field: name, hlc: clock, value: value))
                let (bytes, _) = try await vault.seal(data, object: object, purpose: .registryField, maximum: 8 * 1024, client: client)
                output[name] = .object(["e1": .string(bytes.base64EncodedString())])
            }
            sealed.ops[index].set = output
        }
        return sealed
    }

    func open(_ rows: [RegistryRow]) async throws -> [RegistryRow] {
        var output: [RegistryRow] = []
        for var row in rows {
            guard !row.deleted, row.delHlc == nil else { throw MobileVaultError.verification }
            var fields: [String: JSONValue] = [:]
            var clocks: [String: String] = [:]
            for (name, value) in row.fields {
                guard let envelope = value.objectValue, envelope.count == 1,
                      let text = envelope["e1"]?.stringValue, text.utf8.count <= 24 * 1024,
                      let bytes = Data(base64Encoded: text), let clock = row.clocks[name] else { throw MobileVaultError.verification }
                let plaintext = try await vault.open(bytes, object: object, purpose: .registryField, maximum: 8 * 1024, client: client)
                let field = try JSONDecoder().decode(Field.self, from: plaintext)
                guard field.kind == row.kind, field.id == row.id, field.field == name, field.hlc == clock else { throw MobileVaultError.verification }
                fields[name] = field.value
                clocks[name] = clock
            }
            row.fields = fields
            row.clocks = clocks
            output.append(row)
        }
        return output
    }

    static func merge(_ rows: [RegistryRow], into baseline: [String: [String: RegistryRow]]) -> [RegistryRow] {
        rows.map { row in
            var merged = baseline[row.kind]?[row.id] ?? row
            for (name, value) in row.fields {
                guard let clock = row.clocks[name], hlcNewer(clock, merged.clocks[name]) || baseline[row.kind]?[row.id] == nil else { continue }
                if value.isNull { merged.fields.removeValue(forKey: name) }
                else { merged.fields[name] = value }
                merged.clocks[name] = clock
            }
            merged.seq = row.seq
            return merged
        }
    }
}
