import CryptoKit
import Foundation

enum MobileVaultError: LocalizedError {
    case unavailable, notApproved, verification, storage, oversized, conflict
    case http(Int)

    var errorDescription: String? {
        switch self {
        case .unavailable: return "Vault connection or keys are unavailable."
        case .notApproved: return "Approve this device before using encrypted sync."
        case .verification: return "Vault verification failed. Sync remains paused."
        case .storage: return "Secure vault storage is unavailable. Existing data was retained."
        case .oversized: return "Vault data exceeds the supported size."
        case .conflict: return "The vault changed concurrently. The pending operation was retained."
        case .http(let status): return "Vault request failed (HTTP \(status))."
        }
    }
}

struct MobileVaultClient: Sendable {
    let origin: URL
    let orgId: String
    let token: @Sendable () async -> String?
    var transport: @Sendable (URLRequest) async throws -> (Data, Int) = { request in
        let (stream, response) = try await URLSession.shared.bytes(for: request)
        guard let response = response as? HTTPURLResponse else { throw MobileVaultError.unavailable }
        if response.expectedContentLength > Int64(VaultPersistence.maxBytes) { throw MobileVaultError.oversized }
        var data = Data()
        for try await byte in stream {
            guard data.count < VaultPersistence.maxBytes else { throw MobileVaultError.oversized }
            data.append(byte)
        }
        return (data, response.statusCode)
    }

    func request(_ path: String, method: String = "GET", body: Data? = nil,
                 contentType: String = "application/octet-stream") async throws -> (Data, Int) {
        guard let bearer = await token() else { throw MobileVaultError.unavailable }
        var components = URLComponents(url: origin.appending(path: "vault/\(orgId)"), resolvingAgainstBaseURL: false)!
        let pieces = path.split(separator: "?", maxSplits: 1, omittingEmptySubsequences: false)
        components.path += String(pieces[0])
        if pieces.count == 2 { components.percentEncodedQuery = String(pieces[1]) }
        guard let url = components.url else { throw MobileVaultError.unavailable }
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.httpMethod = method
        request.httpBody = body
        request.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization")
        request.setValue(contentType, forHTTPHeaderField: "Content-Type")
        let result = try await transport(request)
        guard result.0.count <= VaultPersistence.maxBytes else { throw MobileVaultError.oversized }
        return result
    }
}

enum MobileVaultPhase: String, Sendable {
    case checking, legacy, notEnrolled, pending, ready, locked, keyUpdateRequired, verificationFailed, revoked, recoveryConfirmationRequired
}

struct MobileVaultStatus: Sendable {
    var phase: MobileVaultPhase
    var fingerprint: String?
    var membershipHash: String?
    var deviceId: String?
    var epoch: UInt64?
    var pairingCode: String?
    var message: String?
}

actor MobileVault {
    private struct Identity: Codable {
        var id: Data
        var signingSeed: Data
        var agreementSeed: Data
        var signingKey: Curve25519.Signing.PrivateKey { get throws { try .init(rawRepresentation: signingSeed) } }
        var agreementKey: Curve25519.KeyAgreement.PrivateKey { get throws { try .init(rawRepresentation: agreementSeed) } }
        var entry: VaultDeviceEntry {
            get throws { try .init(deviceId: id, signingKey: signingKey.publicKey.rawRepresentation,
                                  encryptionKey: agreementKey.publicKey.rawRepresentation, status: .active) }
        }
    }
    private struct ObjectKey: Codable {
        var object: Data
        var epoch: UInt64
        var id: Data
        var bytes: Data
    }
    private struct State: Codable {
        var version = 1
        var required = true
        var identity: Identity?
        var fingerprint: Data?
        var records: [Data] = []
        var keyring: Data?
        var keys: [ObjectKey] = []
        var enrollment: Data?
        var pendingMembership: Data?
        var owed: [Data] = []
        var envelopes: [String: Data] = [:]
        var recoverySecret: Data?
    }
    private struct MembershipPage: Decodable {
        var records: [Data]
        var truncated: Bool
        var headSeq: Int64
    }
    private struct ObjectKeys: Decodable {
        struct Entry: Decodable { var record: Data }
        var keys: [Entry]
    }
    private var state = State()
    private var history: [VaultMembershipState] = []
    private var loaded = false
    private var required = false
    private var phase: MobileVaultPhase = .checking
    private var message: String?
    private var busy = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private let persistence: VaultPersistence
    private let profileHash: Data

    init(persistence: VaultPersistence, orgId: String, userId: String) {
        self.persistence = persistence
        profileHash = VaultPolicy.profileHash(orgId: orgId, userId: userId)
    }

    func status() -> MobileVaultStatus {
        var code: String?
        if let identity = state.identity, let requestId = state.enrollment,
           let head = history.last, let entry = try? identity.entry {
            code = VaultEnrollmentRequest(vaultId: head.vaultId, requestId: requestId, deviceId: identity.id,
                                          signingKey: entry.signingKey, encryptionKey: entry.encryptionKey)
                .pairingCode(genesisHash: head.genesisHash)
        }
        return MobileVaultStatus(phase: phase, fingerprint: state.fingerprint?.vaultHex, membershipHash: history.last?.hash.vaultHex,
                                 deviceId: state.identity?.id.vaultHex, epoch: history.last?.epoch,
                                 pairingCode: code, message: message)
    }

    private func acquire() async throws {
        if busy { await withCheckedContinuation { waiters.append($0) } } else { busy = true }
        if Task.isCancelled { release(); throw CancellationError() }
    }
    private func release() {
        if waiters.isEmpty { busy = false } else { waiters.removeFirst().resume() }
    }

    private func load() throws {
        guard !loaded else { return }
        do {
            required = try persistence.exists || persistence.secrets.load(account: persistence.account) != nil
            if let data = try persistence.load() {
                let saved = try JSONDecoder().decode(State.self, from: data)
                guard saved.version == 1 else { throw MobileVaultError.verification }
                state = saved
                required = true
                try rebuild()
            } else {
                state = State()
                history = []
            }
            loaded = true
        } catch {
            phase = .locked
            throw MobileVaultError.storage
        }
    }

    private func save() throws {
        do { try persistence.save(JSONEncoder().encode(state)) }
        catch { phase = .locked; loaded = false; throw MobileVaultError.storage }
        required = true
    }

    private func rebuild() throws {
        history = try Self.verifyHistory(state.records, fingerprint: state.fingerprint, profileHash: profileHash)
        if let identity = state.identity {
            guard identity.id.count == 16, identity.signingSeed.count == 32, identity.agreementSeed.count == 32 else { throw MobileVaultError.verification }
            _ = try identity.entry
        }
        if let ring = state.keyring { _ = try VaultKeyring.decode(ring) }
    }

    static func verifyHistory(_ records: [Data], fingerprint: Data?, profileHash: Data) throws -> [VaultMembershipState] {
        guard records.count <= 4096 else { throw MobileVaultError.oversized }
        guard let first = records.first else { return [] }
        guard let fingerprint, fingerprint.count == 32, VaultPolicy.membershipHash(first) == fingerprint else { throw MobileVaultError.verification }
        let binding = try VaultUnverifiedRecord.parse(first, maxPayloadBytes: VaultPolicy.maxPolicyBytes).untrustedBinding
        var history = [try VaultMembershipState.fromGenesis(first, expectedVaultId: binding.vaultId,
                                                            expectedGeneration: binding.generation, expectedProfileHash: profileHash)]
        for record in records.dropFirst() { history.append(try history.last!.apply(record)) }
        return history
    }

    private func updatePhase() throws {
        guard phase != .locked && phase != .verificationFailed else { return }
        if state.pendingMembership != nil || !state.owed.isEmpty { phase = .keyUpdateRequired; return }
        guard let head = history.last, let identity = state.identity else {
            phase = required ? .notEnrolled : .legacy
            return
        }
        guard let device = head.device(identity.id) else { phase = .pending; return }
        guard device.status == .active else { phase = .revoked; return }
        guard try device == identity.entry else { throw MobileVaultError.verification }
        guard let encoded = state.keyring, try VaultKeyring.decode(encoded).epochKey(head.epoch) != nil else {
            phase = .keyUpdateRequired
            return
        }
        phase = state.recoverySecret == nil ? .ready : .recoveryConfirmationRequired
    }

    private func fetchHistory(_ client: MobileVaultClient) async throws {
        var records = state.records
        while true {
            let after = records.isEmpty ? "-1" : String(records.count - 1)
            let (data, status) = try await client.request("/membership?after=\(after)")
            guard status == 200 else { throw MobileVaultError.http(status) }
            let page = try JSONDecoder().decode(MembershipPage.self, from: data)
            guard page.records.count <= 256, records.count + page.records.count <= 4096 else { throw MobileVaultError.oversized }
            records.append(contentsOf: page.records)
            if !page.truncated { break }
            guard !page.records.isEmpty else { throw MobileVaultError.verification }
        }
        let verified = try Self.verifyHistory(records, fingerprint: state.fingerprint, profileHash: profileHash)
        if records != state.records {
            state.records = records
            try save()
        }
        history = verified
    }

    private func envelopeContext(_ encoded: Data, object: Data) throws -> (VaultRecordBinding, Data) {
        let binding = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: VaultEnvelope.maxPayloadBytes).untrustedBinding
        guard binding.kind == .keyEnvelope, binding.objectId == object,
              let revision = history.first(where: { $0.hash == binding.membershipHash }),
              binding.epoch <= revision.epoch, let author = revision.activeDevice(binding.authorId) else { throw MobileVaultError.verification }
        return (revision.envelopeBinding(objectId: object, epoch: binding.epoch, authorId: author.deviceId), author.signingKey)
    }

    private func fetchKeyring(_ client: MobileVaultClient) async throws {
        guard let identity = state.identity, let head = history.last,
              head.activeDevice(identity.id) != nil else { return }
        if let ring = state.keyring, try VaultKeyring.decode(ring).epochKey(head.epoch) != nil { return }
        let (data, status) = try await client.request("/envelopes/\(identity.id.vaultHex)")
        if status == 404 { return }
        guard status == 200 else { throw MobileVaultError.http(status) }
        let (binding, publicKey) = try envelopeContext(data, object: VaultPolicy.policyObjectId)
        let incoming = try VaultEnvelope.openKeyring(data, expected: binding, recipientKind: .device,
                                                     recipientId: identity.id, recipientKey: identity.agreementKey, trustedPublicKey: publicKey)
        var ring = try state.keyring.map(VaultKeyring.decode) ?? VaultKeyring()
        try ring.merge(incoming)
        state.keyring = ring.encode()
        state.enrollment = nil
        try save()
    }

    func refresh(client: MobileVaultClient) async -> MobileVaultStatus {
        do {
            try await acquire()
            defer { release() }
            try load()
            if phase == .locked { phase = .checking }
            if required { try updatePhase() }
            if state.pendingMembership != nil { try await finishMutation(client) }
            let (_, status) = try await client.request("")
            if status == 404 {
                guard !required else { throw MobileVaultError.verification }
                phase = .legacy
                return self.status()
            }
            guard status == 200 else { throw MobileVaultError.http(status) }
            required = true
            try save()
            if state.fingerprint != nil {
                try await fetchHistory(client)
                try await fetchKeyring(client)
                if !state.owed.isEmpty { try await finishMutation(client) }
            }
            try updatePhase()
            message = nil
        } catch {
            message = error is CancellationError ? nil : "Vault synchronization is paused. Retry when keys and connectivity are available."
            if error is VaultPolicyError || error is VaultRecordError || error is VaultEnvelopeError {
                phase = .verificationFailed
            } else if let error = error as? MobileVaultError, case .verification = error { phase = .verificationFailed }
        }
        return status()
    }

    private func freshIdentity() throws -> Identity {
        Identity(id: try VaultContentCrypto.randomBytes(16), signingSeed: Curve25519.Signing.PrivateKey().rawRepresentation,
                 agreementSeed: Curve25519.KeyAgreement.PrivateKey().rawRepresentation)
    }

    func enroll(fingerprint: Data, client: MobileVaultClient) async throws {
        try await acquire()
        defer { release() }
        try load()
        guard fingerprint.count == 32, state.identity == nil || phase == .pending || phase == .notEnrolled else { throw MobileVaultError.notApproved }
        phase = .pending
        required = true
        if let pinned = state.fingerprint, pinned != fingerprint { throw MobileVaultError.verification }
        state.fingerprint = fingerprint
        try await fetchHistory(client)
        guard let head = history.last else { throw MobileVaultError.verification }
        if state.identity == nil { state.identity = try freshIdentity() }
        if state.enrollment == nil { state.enrollment = try VaultContentCrypto.randomBytes(16) }
        try save()
        let identity = state.identity!
        let entry = try identity.entry
        let enrollment = VaultEnrollmentRequest(vaultId: head.vaultId, requestId: state.enrollment!, deviceId: identity.id,
                                                signingKey: entry.signingKey, encryptionKey: entry.encryptionKey)
        let body: [String: String] = ["requestId": enrollment.requestId.vaultHex, "deviceId": identity.id.vaultHex,
                                     "signingKey": entry.signingKey.vaultHex, "encryptionKey": entry.encryptionKey.vaultHex,
                                     "proof": try identity.signingKey.signature(for: enrollment.proofInput).vaultHex]
        let (_, status) = try await client.request("/enroll", method: "POST", body: JSONEncoder().encode(body), contentType: "application/json")
        guard status == 200 || status == 201 else { throw MobileVaultError.http(status) }
    }

    func recover(kit: String, fingerprint: Data, client: MobileVaultClient) async throws {
        try await acquire()
        defer { release() }
        try load()
        guard phase != .ready, state.pendingMembership == nil, fingerprint.count == 32 else { throw MobileVaultError.notApproved }
        let recovery = try VaultRecoverySecret(kit: kit)
        if let pinned = state.fingerprint, pinned != fingerprint { throw MobileVaultError.verification }
        state.fingerprint = fingerprint
        try await fetchHistory(client)
        guard let head = history.last, try recovery.authorityId() == head.recoveryAuthorityId else { throw MobileVaultError.verification }
        let (data, status) = try await client.request("/envelopes/\(head.recoveryAuthorityId.vaultHex)")
        guard status == 200 else { throw MobileVaultError.http(status) }
        let (binding, publicKey) = try envelopeContext(data, object: VaultPolicy.policyObjectId)
        var ring = try VaultEnvelope.openKeyring(data, expected: binding, recipientKind: .recovery, recipientId: head.recoveryAuthorityId,
                                                 recipientKey: recovery.encryptionKey(), trustedPublicKey: publicKey)
        guard head.epoch < UInt64.max, head.sequence < UInt64.max else { throw MobileVaultError.verification }
        let identity = try freshIdentity()
        var devices = head.devices
        devices.append(try identity.entry)
        let payload = VaultPolicyPayload(sequence: head.sequence + 1, parentHash: head.hash, profileHash: profileHash,
                                         epoch: head.epoch + 1, operation: .recoveryTransition, recoverySigningKey: head.recoverySigningKey,
                                         recoveryEncryptionKey: head.recoveryEncryptionKey, devices: devices)
        let policyBinding = VaultPolicy.policyBinding(vaultId: head.vaultId, generation: head.generation, epoch: payload.epoch,
                                                      authorId: head.recoveryAuthorityId, parent: head.hash)
        let record = try VaultEnvelope.sign(binding: policyBinding, revision: VaultContentCrypto.randomBytes(16),
                                            payload: payload.encode(), signingKey: recovery.signingKey(), limit: VaultPolicy.maxPolicyBytes)
        _ = try head.apply(record)
        try ring.insert(epoch: payload.epoch, key: VaultContentCrypto.randomBytes(32))
        state.identity = identity
        state.records.append(record)
        state.keyring = ring.encode()
        state.pendingMembership = record
        state.enrollment = nil
        state.owed = devices.filter { $0.status == .active }.map(\.deviceId) + [head.recoveryAuthorityId]
        phase = .keyUpdateRequired
        try save()
        try rebuild()
        try await finishMutation(client)
        try updatePhase()
    }

    private func finishMutation(_ client: MobileVaultClient) async throws {
        if let record = state.pendingMembership {
            guard let head = history.last, head.hash == VaultPolicy.membershipHash(record) else { throw MobileVaultError.verification }
            let (_, status) = try await client.request("/membership", method: "POST", body: record)
            if status != 200 {
                let (data, code) = try await client.request("/membership?after=\(Int64(head.sequence) - 1)")
                guard code == 200, try JSONDecoder().decode(MembershipPage.self, from: data).records.first == record else { throw MobileVaultError.conflict }
            }
            state.pendingMembership = nil
            try save()
        }
        guard let head = history.last, let identity = state.identity, let ringBytes = state.keyring else { return }
        let ring = try VaultKeyring.decode(ringBytes)
        for recipient in state.owed {
            let kind: VaultRecipientKind = recipient == head.recoveryAuthorityId ? .recovery : .device
            let publicBytes = kind == .recovery ? head.recoveryEncryptionKey : head.activeDevice(recipient)?.encryptionKey
            guard let publicBytes else { throw MobileVaultError.verification }
            if state.envelopes[recipient.vaultHex] == nil {
                state.envelopes[recipient.vaultHex] = try VaultEnvelope.sealKeyring(
                    binding: head.envelopeBinding(objectId: VaultPolicy.policyObjectId, epoch: head.epoch, authorId: identity.id),
                    kind: kind, recipientId: recipient, recipientKey: .init(rawRepresentation: publicBytes),
                    keyring: ring, signingKey: identity.signingKey)
                try save()
            }
            let (_, status) = try await client.request("/envelopes/\(recipient.vaultHex)", method: "PUT", body: state.envelopes[recipient.vaultHex])
            guard status == 200 else { throw MobileVaultError.http(status) }
            state.owed.removeAll { $0 == recipient }
            state.envelopes.removeValue(forKey: recipient.vaultHex)
            try save()
        }
    }

    static func encryptedRoomId(_ chatId: String) -> String {
        if chatId.utf8.count <= 125 { return chatId + "-e1" }
        return "e1-" + Data(SHA256.hash(data: Data("zeron/encrypted-room/v1\0\(chatId)".utf8)).prefix(20)).vaultHex
    }

    static func objectId(kind: String, id: String) -> Data {
        Data(SHA256.hash(data: Data("zeron/object-id/v1\0\(kind)\0\(id)".utf8)).prefix(16))
    }

    private func fetchObject(_ object: Data, client: MobileVaultClient) async throws {
        guard let ringBytes = state.keyring else { throw MobileVaultError.notApproved }
        let ring = try VaultKeyring.decode(ringBytes)
        let (data, status) = try await client.request("/objects/\(object.vaultHex)/keys")
        guard status == 200 else { throw MobileVaultError.http(status) }
        let entries = try JSONDecoder().decode(ObjectKeys.self, from: data).keys
        guard entries.count <= 1024 else { throw MobileVaultError.oversized }
        for entry in entries {
            let (binding, publicKey) = try envelopeContext(entry.record, object: object)
            guard let epochKey = ring.epochKey(binding.epoch) else { continue }
            let key = try VaultEnvelope.unwrapObjectKey(entry.record, expected: binding, epochKey: epochKey, trustedPublicKey: publicKey)
            if let existing = state.keys.first(where: { $0.object == object && $0.epoch == binding.epoch }) {
                guard existing.id == key.identifier, existing.bytes == key.exposeSecret() else { throw MobileVaultError.verification }
            } else {
                state.keys.append(ObjectKey(object: object, epoch: binding.epoch, id: key.identifier, bytes: key.exposeSecret()))
            }
        }
        try save()
    }

    func open(_ encoded: Data, object: Data, purpose: VaultContentPurpose, maximum: Int,
              client: MobileVaultClient) async throws -> Data {
        try await acquire()
        defer { release() }
        try load()
        guard phase == .ready || phase == .revoked else { throw MobileVaultError.notApproved }
        let untrusted = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: maximum + 144).untrustedBinding
        if !history.contains(where: { $0.hash == untrusted.membershipHash }) { try await fetchHistory(client) }
        guard let revision = history.first(where: { $0.hash == untrusted.membershipHash }), untrusted.epoch == revision.epoch,
              untrusted.objectId == object, let author = revision.activeDevice(untrusted.authorId) else { throw MobileVaultError.verification }
        if !state.keys.contains(where: { $0.object == object && $0.epoch == untrusted.epoch }) { try await fetchObject(object, client: client) }
        guard let stored = state.keys.first(where: { $0.object == object && $0.epoch == untrusted.epoch }) else { throw MobileVaultError.unavailable }
        let binding = revision.contentBinding(objectId: object, authorId: author.deviceId)
        let key = try VaultContentKey(scope: VaultKeyScope(binding), identifier: stored.id, bytes: stored.bytes)
        return try VaultContentCrypto.open(encoded, expected: binding, purpose: purpose, key: key,
                                           trustedPublicKey: author.signingKey, maxPlaintextBytes: maximum).plaintext
    }

    func prepareBatch(_ encoded: Data, object: Data, maximum: Int, client: MobileVaultClient) async throws -> (Data, String) {
        let plaintext = try await open(encoded, object: object, purpose: .chatUpdate, maximum: maximum, client: client)
        let record = try VaultUnverifiedRecord.parse(encoded, maxPayloadBytes: maximum + 144)
        if phase == .ready, let head = history.last, let identity = state.identity,
           record.untrustedBinding == head.contentBinding(objectId: object, authorId: identity.id) {
            return (encoded, record.untrustedRevisionId.vaultHex)
        }
        return try await seal(plaintext, object: object, purpose: .chatUpdate, maximum: maximum, client: client)
    }

    func seal(_ plaintext: Data, object: Data, purpose: VaultContentPurpose, maximum: Int,
              client: MobileVaultClient) async throws -> (Data, String) {
        try await acquire()
        defer { release() }
        guard phase == .ready, let head = history.last, let identity = state.identity,
              head.activeDevice(identity.id) != nil else { throw MobileVaultError.notApproved }
        if !state.keys.contains(where: { $0.object == object && $0.epoch == head.epoch }) { try await fetchObject(object, client: client) }
        if !state.keys.contains(where: { $0.object == object && $0.epoch == head.epoch }) {
            guard let bytes = state.keyring, let epochKey = try VaultKeyring.decode(bytes).epochKey(head.epoch) else { throw MobileVaultError.unavailable }
            let binding = head.envelopeBinding(objectId: object, epoch: head.epoch, authorId: identity.id)
            let key = try VaultContentKey(scope: VaultKeyScope(binding), identifier: VaultContentCrypto.randomBytes(16), bytes: VaultContentCrypto.randomBytes(32))
            let envelope = try VaultEnvelope.wrapObjectKey(binding: binding, epochKey: epochKey, key: key, signingKey: identity.signingKey)
            let (_, status) = try await client.request("/objects/\(object.vaultHex)/keys", method: "PUT", body: envelope)
            guard status == 200 || status == 409 else { throw MobileVaultError.http(status) }
            try await fetchObject(object, client: client)
        }
        guard let stored = state.keys.first(where: { $0.object == object && $0.epoch == head.epoch }) else { throw MobileVaultError.unavailable }
        let binding = head.contentBinding(objectId: object, authorId: identity.id)
        let record = try VaultContentCrypto.seal(binding: binding, purpose: purpose,
            key: VaultContentKey(scope: VaultKeyScope(binding), identifier: stored.id, bytes: stored.bytes),
            signer: VaultDeviceSigner(authorId: identity.id, seed: identity.signingSeed), plaintext: plaintext, maxPlaintextBytes: maximum)
        return (record.encoded, record.revisionId.vaultHex)
    }
}
