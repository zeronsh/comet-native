// Workspace doc mirror — the iOS analogue of the desktop's comet-doc mirror
// over the workspace doc (crates/doc/src/workspace.rs). Joins the per-user
// `ws3/{orgId}/{userId}` room, projects the doc into typed rows, and performs
// the writes the writer discipline allows a viewer device: chat creates,
// archives, seen marks, plus its own device row and presence heartbeat.

import Foundation
import Loro
import Observation

@MainActor
@Observable
final class WorkspaceStore {
    private(set) var devices: [DeviceRow] = []
    private(set) var spaces: [Space] = []
    private(set) var chats: [Chat] = []
    private(set) var sessions: [String: SessionRow] = [:]
    private(set) var presence: [String: Int64] = [:]  // deviceId → last heartbeat ms
    private(set) var connected = false

    let doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    private var heartbeatTask: Task<Void, Never>?
    private let config: AppConfig

    init(config: AppConfig) {
        self.config = config
    }

    func start() {
        guard room == nil else { return }
        let roomId = "ws3/\(config.orgId)/\(config.userId)"
        let client = RoomClient(roomId: roomId, doc: doc) { [config] in
            await config.workspaceSocketURL()
        } events: { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
        room = client

        // Local commits → room. The subscription fires synchronously inside
        // commit; hop to the actor to send.
        let localSub = doc.subscribeLocalUpdate { [weak client] update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes) }
        }
        subscriptions.append(localSub)

        Task { await client.start() }
        startHeartbeat(client: client)
        registerOwnDevice()
        project()
    }

    func stop() {
        heartbeatTask?.cancel()
        heartbeatTask = nil
        subscriptions.removeAll()
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
        case .ephemeralUpdate:
            projectPresence()
        }
    }

    // MARK: Presence

    private func startHeartbeat(client: RoomClient) {
        heartbeatTask = Task { [config] in
            while !Task.isCancelled {
                let key = "presence/\(config.deviceId)"
                await client.eph.set(key: key, value: nowMs())
                let delta = await client.eph.encode(key: key)
                await client.sendEphemeralUpdate([UInt8](delta))
                try? await Task.sleep(nanoseconds: 15_000_000_000)  // PRESENCE_INTERVAL_MS
            }
        }
    }

    private func projectPresence() {
        guard let room else { return }
        Task { @MainActor in
            let states = await room.eph.getAllStates()
            var fresh: [String: Int64] = [:]
            for (key, value) in states where key.hasPrefix("presence/") {
                if let ms = value.i64Value {
                    fresh[String(key.dropFirst("presence/".count))] = ms
                }
            }
            presence = fresh
        }
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        guard let ms = presence[deviceId] else { return false }
        return nowMs() - ms < presenceFreshMs
    }

    // MARK: Projection (doc → rows)

    private func project() {
        let value = doc.getDeepValue()
        guard let root = value.mapValue else { return }

        devices = (root["devices"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue else { return nil }
            return DeviceRow(id: id,
                            name: m["name"]?.stringValue ?? id,
                            platform: m["platform"]?.stringValue ?? "",
                            lastSeenAt: m["lastSeenAt"]?.i64Value,
                            createdAt: m["createdAt"]?.i64Value)
        }.sorted { $0.name < $1.name }

        spaces = (root["spaces"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue,
                  let path = m["path"]?.stringValue else { return nil }
            return Space(id: id, deviceId: deviceId, path: path,
                         name: m["name"]?.stringValue,
                         gitDetected: m["gitDetected"]?.boolValue ?? false,
                         gitCheckedAt: m["gitCheckedAt"]?.i64Value,
                         checkoutId: m["checkoutId"]?.stringValue,
                         createdAt: m["createdAt"]?.i64Value ?? 0)
        }.sorted { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }  // creation order, id tiebreak

        chats = (root["chats"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue else { return nil }
            var chatConfig: ChatConfig?
            if let c = m["config"]?.mapValue {
                chatConfig = ChatConfig(harness: c["harness"]?.stringValue ?? "claude-code",
                                        model: c["model"]?.stringValue,
                                        reasoning: c["reasoning"]?.stringValue,
                                        sandbox: c["sandbox"]?.stringValue)
            }
            return Chat(id: id, deviceId: deviceId,
                        title: m["title"]?.stringValue,
                        archived: m["archived"]?.boolValue ?? false,
                        cwd: m["cwd"]?.stringValue,
                        branch: m["branch"]?.stringValue,
                        checkoutId: m["checkoutId"]?.stringValue,
                        config: chatConfig,
                        lastMessagePreview: m["lastMessagePreview"]?.stringValue,
                        lastMessageAt: m["lastMessageAt"]?.i64Value,
                        createdAt: m["createdAt"]?.i64Value ?? 0,
                        spaceId: m["spaceId"]?.stringValue,
                        lastSeenAt: m["lastSeenAt"]?.i64Value)
        }

        var rows: [String: SessionRow] = [:]
        for (_, v) in root["sessions"]?.mapValue ?? [:] {
            guard let m = v.mapValue, let chatId = m["chatId"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue,
                  let statusStr = m["status"]?.stringValue,
                  let status = SessionStatus(rawValue: statusStr) else { continue }
            rows[chatId] = SessionRow(chatId: chatId, deviceId: deviceId, status: status,
                                      startedAt: m["startedAt"]?.i64Value,
                                      updatedAt: m["updatedAt"]?.i64Value ?? 0)
        }
        sessions = rows
    }

    // MARK: Derived views

    /// state.rs `overview_chats`: every non-archived chat of a live space,
    /// attention-sorted.
    var overviewChats: [Chat] {
        let liveSpaceIds = Set(spaces.map(\.id))
        let live = chats.filter { !$0.archived && $0.spaceId.map(liveSpaceIds.contains) == true }
        return attentionSort(live) { indicator(for: $0) }
    }

    func chats(in spaceId: String) -> [Chat] {
        chats.filter { !$0.archived && $0.spaceId == spaceId }
            .sorted { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        chatIndicator(chat: chat, live: effectiveStatus(sessions[chat.id], now: nowMs()))
    }

    /// Aggregate most-urgent member status for a space's leading dot.
    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        let members = chats(in: spaceId).map { indicator(for: $0) }
        return members.min(by: { $0.rawValue < $1.rawValue })
    }

    // MARK: Device relay (folder browsing / direct host RPCs)

    @ObservationIgnored private var relayClients: [String: DeviceRelayClient] = [:]

    private func relay(for deviceId: String) -> DeviceRelayClient {
        if let existing = relayClients[deviceId] { return existing }
        let client = DeviceRelayClient(deviceId: deviceId, config: config)
        relayClients[deviceId] = client
        return client
    }

    /// ListFolders on the target device (engine caps at 500 entries, hides
    /// dotfiles, stamps isRepo). nil path = the device's home directory.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        var params: [String: Any] = [:]
        if let path { params["path"] = path }
        return try? await relay(for: deviceId).call(method: "ListFolders", params: params)
    }

    /// ListRefs on the target device — branches with current/worktree markers
    /// (default branch first, per the engine's ordering).
    func listRefs(deviceId: String, repoPath: String) async -> [RepoRef]? {
        try? await relay(for: deviceId).call(method: "ListRefs", params: ["repoPath": repoPath])
    }

    /// SwitchRef — `git checkout` in the given folder on the target device.
    /// Returns git's error message on failure (dirty tree, held ref, …).
    func switchRef(deviceId: String, repoPath: String, refName: String) async -> String? {
        struct Reply: Decodable { var branch: String? }
        do {
            let _: Reply = try await relay(for: deviceId)
                .call(method: "SwitchRef", params: ["repoPath": repoPath, "refName": refName])
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// CreateWorktree — a fresh isolated worktree off the base ref; returns
    /// its path.
    func createWorktree(deviceId: String, repoPath: String, branch: String) async -> String? {
        struct Reply: Decodable { var path: String }
        let reply: Reply? = try? await relay(for: deviceId)
            .call(method: "CreateWorktree", params: ["repoPath": repoPath, "branch": branch])
        return reply?.path
    }

    /// Retarget a session onto another checkout (the desktop's
    /// setChatCwd/setChatBranch mutates — LWW row writes here).
    func setChatCheckout(chatId: String, cwd: String, branch: String) {
        updateChat(chatId) { row in
            try row.insert(key: "cwd", v: cwd)
            try row.insert(key: "branch", v: branch)
        }
    }

    // MARK: Writes (viewer-device discipline)

    private func registerOwnDevice() {
        let map = doc.getMap(id: "devices")
        do {
            let row = try map.getOrCreateContainer(key: config.deviceId, child: LoroMap())
            try row.insert(key: "id", v: config.deviceId)
            try row.insert(key: "name", v: config.deviceName)
            try row.insert(key: "platform", v: "ios")
            try row.insert(key: "lastSeenAt", v: nowMs())
            if row.get(key: "createdAt") == nil {
                try row.insert(key: "createdAt", v: nowMs())
            }
            doc.commit()
        } catch {
            // Registration is cosmetic; sync continues without it.
        }
    }

    /// Mint a new chat onto a space (workspace_host.rs create_chat shape).
    /// The host = the space's owning device picks it up via the doc.
    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) -> String {
        let chatId = UUID().uuidString.lowercased()
        let map = doc.getMap(id: "chats")
        do {
            let row = try map.getOrCreateContainer(key: chatId, child: LoroMap())
            try row.insert(key: "id", v: chatId)
            try row.insert(key: "deviceId", v: space.deviceId)
            try row.insert(key: "archived", v: false)
            try row.insert(key: "cwd", v: cwd ?? space.path)
            try row.insert(key: "spaceId", v: space.id)
            try row.insert(key: "createdAt", v: nowMs())
            if let branch {
                try row.insert(key: "branch", v: branch)
            }
            if let cfg = LoroValue.fromEncodable(chatConfig) {
                try row.insert(key: "config", v: cfg)
            }
            doc.commit()
            project()
        } catch {}
        return chatId
    }

    /// Create a space. Preferred path: `Mutate {op:createSpace}` straight to
    /// the owning host over its relay (it applies the row to its own workspace
    /// doc, functionally identical to the desktop's local mutate + sync).
    /// Fallback when the host is unreachable: LWW row write into our mirror —
    /// creates are legal from any device; the owner stamps git on arrival.
    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String {
        // Dedup on (device, path) like the desktop palette.
        if let existing = spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
            return existing.id
        }
        let spaceId = UUID().uuidString.lowercased()
        struct OkReply: Decodable { var ok: Bool? }
        let params: [String: Any] = [
            "op": "createSpace",
            "spaceId": spaceId,
            "deviceId": deviceId,
            "path": path,
            "gitDetected": gitDetected,
        ]
        let viaHost: OkReply? = try? await relay(for: deviceId).call(method: "Mutate", params: params)
        if viaHost == nil {
            let map = doc.getMap(id: "spaces")
            do {
                let row = try map.getOrCreateContainer(key: spaceId, child: LoroMap())
                try row.insert(key: "id", v: spaceId)
                try row.insert(key: "deviceId", v: deviceId)
                try row.insert(key: "path", v: path)
                try row.insert(key: "gitDetected", v: gitDetected)
                try row.insert(key: "createdAt", v: nowMs())
                doc.commit()
            } catch {}
        }
        project()
        return spaceId
    }

    func setArchived(chatId: String, archived: Bool) {
        updateChat(chatId) { row in
            try row.insert(key: "archived", v: archived)
        }
    }

    func markSeen(chatId: String) {
        updateChat(chatId) { row in
            try row.insert(key: "lastSeenAt", v: nowMs())
        }
    }

    func rename(chatId: String, title: String) {
        updateChat(chatId) { row in
            try row.insert(key: "title", v: title)
        }
    }

    /// Chat config is an LWW map set on the chat row; the host reads it when
    /// dispatching the next run.
    func setChatConfig(chatId: String, config chatConfig: ChatConfig) {
        updateChat(chatId) { row in
            if let value = LoroValue.fromEncodable(chatConfig) {
                try row.insert(key: "config", v: value)
            }
        }
    }

    private func updateChat(_ chatId: String, _ mutate: (LoroMap) throws -> Void) {
        let map = doc.getMap(id: "chats")
        guard let row = map.get(key: chatId)?.asLoroMap() else { return }
        do {
            try mutate(row)
            doc.commit()
            project()
        } catch {}
    }
}
