// Session doc mirror — transcript entries + the durable command queue for one
// chat (crates/doc/src/schema.rs). A viewer device never writes message
// entries; it appends command ledger entries (rule 1) and lets the host drain
// them. Optimistic echo: pending sends render locally under their client-minted
// message id until the host writes the real entry with the same id.

import Foundation
import Loro
import Observation

@MainActor
@Observable
final class SessionStore {
    let chatId: String
    /// The chat's host device — nudge target for cold-host command drains.
    var hostDeviceId: String?
    private(set) var entries: [MessageEntry] = []
    private(set) var connected = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [(messageId: String, text: String, at: Int64)] = []

    let doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    private let config: AppConfig

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?

    init(chatId: String, config: AppConfig, offline: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.offline = offline
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        entries = new
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil, !offline else { return }
        // Local-first: last-synced transcript renders instantly (even when the
        // host device is offline); the join backfills incrementally from here.
        if DocDisk.load(into: doc, id: chatId) {
            project()
        }
        saver = DocSaver(docId: chatId, doc: doc)
        let client = RoomClient(roomId: chatId, doc: doc) { [config, chatId] in
            await config.sessionSocketURL(chatId: chatId)
        } events: { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
        room = client
        let localSub = doc.subscribeLocalUpdate { [weak client, weak self] update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes) }
            Task { @MainActor [weak self] in self?.saver?.poke() }
        }
        subscriptions.append(localSub)
        Task { await client.start() }
        project()
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    func stop() {
        subscriptions.removeAll()
        saver?.flush()
        if let room {
            Task { await room.stop() }
        }
        room = nil
        connected = false
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            connected = true
            project()
        case .disconnected:
            connected = false
        case .remoteUpdate:
            project()
            saver?.poke()
        case .ephemeralUpdate:
            break
        }
    }

    // MARK: Projection

    private func project() {
        let value = doc.getDeepValue()
        guard let root = value.mapValue else { return }
        let raw = (root["messages"]?.listValue ?? []).compactMap(Self.entryFrom)
        entries = Self.joinContinuations(raw)
        // Drop echoes the host has materialized.
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
    }

    private static func entryFrom(_ value: LoroValue) -> MessageEntry? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let roleStr = m["role"]?.stringValue,
              let role = MessageRole(rawValue: roleStr) else { return nil }
        let parts = (m["parts"]?.listValue ?? []).compactMap(partFrom)
        return MessageEntry(id: id, role: role, parts: parts,
                            createdAt: m["createdAt"]?.i64Value ?? 0,
                            deviceId: m["deviceId"]?.stringValue ?? "",
                            status: m["status"]?.stringValue.flatMap(MessageStatus.init(rawValue:)),
                            continuationOf: m["continuationOf"]?.stringValue)
    }

    private static func partFrom(_ value: LoroValue) -> MessagePart? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let kind = m["kind"]?.stringValue else { return nil }
        switch kind {
        case "text":
            return .text(id: id, text: m["text"]?.stringValue ?? "")
        case "tool":
            guard let callMap = m["call"]?.mapValue else { return nil }
            let tag = callMap["kind"]?.stringValue ?? "unknown"
            var fields: [String: AnyHashable] = [:]
            for (k, v) in callMap where k != "kind" {
                if let s = v.stringValue { fields[k] = s }
                else if let b = v.boolValue { fields[k] = b }
                else if let i = v.i64Value { fields[k] = i }
                else if let list = v.listValue {
                    // ApplyPatch changes / Todo items — keep a JSON echo.
                    fields[k] = list.map { "\($0.jsonObject)" }
                }
            }
            // isError presence IS the resolution marker (schema.rs:96).
            let isError = m["isError"]?.boolValue
            return .tool(id: id, call: RenderToolCall(tag: tag, fields: fields),
                         isError: isError ?? false, resolved: isError != nil)
        case "input":
            var questions: [UserInputQuestion] = []
            if let list = m["questions"]?.listValue,
               let data = try? JSONSerialization.data(withJSONObject: list.map(\.jsonObject)),
               let decoded = try? JSONDecoder().decode([UserInputQuestion].self, from: data) {
                questions = decoded
            }
            return .input(id: id, requestId: id, questions: questions,
                          resolved: m["resolved"]?.boolValue ?? false)
        case "error":
            return .error(id: id, message: m["message"]?.stringValue ?? "")
        default:
            return nil
        }
    }

    /// schema.rs join_continuation_entries: concatenate continuation parts onto
    /// the root in list order; orphans surface standalone.
    static func joinContinuations(_ raw: [MessageEntry]) -> [MessageEntry] {
        var roots: [MessageEntry] = []
        var index: [String: Int] = [:]
        for entry in raw {
            if let rootId = entry.continuationOf, let ix = index[rootId] {
                roots[ix].parts.append(contentsOf: entry.parts)
            } else {
                index[entry.id] = roots.count
                roots.append(entry)
            }
        }
        return roots
    }

    // MARK: Derived

    var lastEntryId: String? { entries.last?.id }

    var liveEntry: MessageEntry? {
        entries.last(where: { $0.status == .streaming })
    }

    /// The unresolved input request to surface in the question panel.
    var openInputRequest: (entryId: String, requestId: String, questions: [UserInputQuestion])? {
        for entry in entries.reversed() {
            for part in entry.parts.reversed() {
                if case .input(_, let requestId, let questions, let resolved) = part, !resolved {
                    return (entry.id, requestId, questions)
                }
            }
        }
        return nil
    }

    // MARK: Command plane (ledger rule 1: append-only, own entries only)

    func sendRun(prompt: String, chat: Chat) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let request = RunRequest(prompt: prompt,
                                 model: chat.config?.model,
                                 reasoning: chat.config?.reasoning,
                                 cwd: chat.cwd ?? "",
                                 sandbox: chat.config?.sandbox ?? "workspace-write")
        queueCommand(kind: "run", payload: [
            "kind": "run",
            "request": encodableJSON(request),
            "messageId": messageId,
        ])
        pendingSends.append((messageId, prompt, nowMs()))
    }

    func sendSteer(prompt: String) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        queueCommand(kind: "steer", payload: [
            "kind": "steer",
            "prompt": prompt,
            "messageId": messageId,
        ])
        pendingSends.append((messageId, prompt, nowMs()))
    }

    func sendInterrupt() {
        queueCommand(kind: "interrupt", payload: ["kind": "interrupt"])
    }

    func respondInput(requestId: String, answers: [UserInputAnswer]) {
        queueCommand(kind: "respondInput", payload: [
            "kind": "respondInput",
            "requestId": requestId,
            "answers": answers.map(encodableJSON),
        ])
    }

    /// schema.rs queue_command, field for field.
    private func queueCommand(kind: String, payload: [String: Any]) {
        let commands = doc.getList(id: "commands")
        do {
            let map = try commands.pushContainer(child: LoroMap())
            try map.insert(key: "id", v: UUID().uuidString.lowercased())
            try map.insert(key: "kind", v: kind)
            try map.insert(key: "payload", v: LoroValue.fromJSON(payload))
            try map.insert(key: "issuedBy", v: config.deviceId)
            try map.insert(key: "issuedAt", v: nowMs())
            if let turnId = lastEntryId {
                try map.insert(key: "basedOn", v: LoroValue.map(value: [
                    "turnId": .string(value: turnId),
                    "frontier": .null,
                ]))
            }
            try map.insert(key: "expiresAt", v: nowMs() + commandDefaultTtlMs)
            try map.insert(key: "status", v: "pending")
            doc.commit()
        } catch {}
        nudgeHost()
    }

    /// Durable-nudge the host device so a cold host opens the doc and drains
    /// (doc_host.rs nudge_remote_host). Fire-and-forget; the command is
    /// durable in the doc regardless.
    private func nudgeHost() {
        guard let hostDeviceId else { return }
        Task { [config, chatId] in
            await config.nudge(deviceId: hostDeviceId, chatId: chatId)
        }
    }
}

private func encodableJSON<T: Encodable>(_ value: T) -> Any {
    guard let data = try? JSONEncoder().encode(value),
          let obj = try? JSONSerialization.jsonObject(with: data) else { return [:] }
    return obj
}
