// Workspace registry mirror — the iOS analogue of the desktop's RegistryDoc
// host (crates/doc/src/registry.rs + crates/engine WorkspaceHost). Joins the
// per-user `/registry/{orgId}/ws` room, projects the row table into typed
// rows, and performs the writes the writer discipline allows a viewer device:
// chat creates, archives, renames and seen marks. iOS is a viewport, not an
// engine device, so it owns no device row; it does publish a presence beat
// (registry presence replaced the old ws room's ephemeral store).
//
// Reads are OVERLAY reads: the server's authoritative rows plus the pending
// op-batch queue replayed on top (optimistic local writes, retired on ack).
// Field names are identical to the old Loro rows (camelCase, epoch-ms
// timestamps) so the projection — and every view above it — is unchanged.

import Foundation
import Observation

@MainActor
@Observable
final class WorkspaceStore {
    private(set) var devices: [DeviceRow] = []
    private(set) var spaces: [Space] = []
    private(set) var chats: [Chat] = []
    private(set) var sessions: [String: SessionRow] = [:]
    private(set) var presence: [String: Int64] = [:]  // deviceId → last beat ms
    private(set) var changeRequestSnapshots: [ChangeRequestWatchKey: CheckoutChangeRequestStatus] = [:]
    private(set) var connected = false
    /// True once ANY transport delivered server state this session (socket
    /// state frame or HTTPS pull). Drives the "connecting" spinner: with the
    /// pull-first bootstrap this flips in ~1 round trip, while the socket
    /// spends 4+ on TLS + upgrade + hello — and on networks that strip WS
    /// upgrades it's the only flag that ever would.
    private(set) var synced = false

    /// Presence entries older than this are expired (mirrors the Rust
    /// client's 30s TTL, measured from RECEIPT — beats carry the sender's
    /// wall clock, which we never trust for freshness).
    static let presenceTtlMs: Int64 = 30_000
    /// Dial-gate thresholds (workspace_host.rs PRESENCE_FRESH_MS /
    /// DIAL_GATE_DARK_MS / DIAL_GATE_WARMUP_MS, PR #168).
    static let presenceLiveFreshMs: Int64 = 45_000
    static let dialGateDarkMs: Int64 = 5 * 60_000
    static let dialGateWarmupMs: Int64 = 60_000

    @ObservationIgnored private var doc: RegistryDoc
    @ObservationIgnored private var client: RegistryClient?
    @ObservationIgnored private var saver: RegistrySaver?
    @ObservationIgnored private var presenceReceivedAt: [String: Int64] = [:]
    /// When the registry room (re)joined — the dial-gate warm-up clock
    /// restarts on every rejoin.
    @ObservationIgnored private var registryJoinedAt: Int64?
    private let config: AppConfig

    init(config: AppConfig) {
        self.config = config
        self.doc = RegistryDoc(deviceId: config.deviceId)
    }

    func start() {
        guard client == nil else { return }
        // Local-first: hydrate from the on-device blob before joining — the
        // sidebar renders immediately and the hello backfills from our
        // cursor. First run after the update: no blob → cursor null → the
        // server's full state (the engines already seeded everything).
        let blobURL = DocDisk.registryURL(orgId: config.orgId, userId: config.userId)
        if let data = try? Data(contentsOf: blobURL),
           let loaded = try? RegistryDoc.from(data: data, deviceId: config.deviceId) {
            doc = loaded
        }
        project()
        saver = RegistrySaver(url: blobURL) { [weak self] in
            try? self?.doc.toData()
        }

        let delegate = RegistryClient.Delegate(
            helloCursor: { [weak self] in self?.doc.helloCursor ?? nil },
            takePushable: { [weak self] in self?.doc.takePushable() ?? [] },
            event: { [weak self] event in self?.handle(event) }
        )
        let client = RegistryClient(device: config.deviceId,
                                    urlProvider: { [config] in await config.registrySocketURL() },
                                    delegate: delegate)
        self.client = client
        Task { await client.start() }
        // Pull-first bootstrap + poll-while-down: one HTTPS GET syncs the
        // sidebar in ~1 RTT while the socket dials, and while the socket is
        // down (airplane wifi strips WS upgrades) a 20s poll keeps rows,
        // presence, and pending writes flowing. The GET doubles as our
        // presence beat, so this device stays visible to peers.
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            await self?.pullDelta()
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 20_000_000_000)
                guard let self, !Task.isCancelled else { return }
                if !self.connected {
                    await self.pushPendingOverHTTP()
                    await self.pullDelta()
                }
            }
        }
    }

    @ObservationIgnored private var pollTask: Task<Void, Never>?

    /// The WS hello's delta answer over plain HTTPS, applied through the
    /// exact state path the socket uses.
    private func pullDelta() async {
        guard let request = await config.registryRowsRequest(since: doc.helloCursor) else {
            roomLog.warning("registry: http pull skipped — no URL (token unavailable)")
            return
        }
        let fetched = try? await URLSession.shared.data(for: request)
        guard let (data, response) = fetched, let http = response as? HTTPURLResponse else {
            roomLog.warning("registry: http pull transport error; will retry")
            return
        }
        guard http.statusCode == 200 else {
            roomLog.warning("registry: http pull http=\(http.statusCode); will retry")
            return
        }
        struct PullBody: Decodable {
            var seq: UInt64
            var full: Bool
            var gcFloor: UInt64?
            var rows: [RegistryRow]
            var presence: [String: Int64]?
        }
        guard let body = try? JSONDecoder().decode(PullBody.self, from: data) else {
            roomLog.warning("registry: http pull body unparseable")
            return
        }
        handle(.state(seq: body.seq, full: body.full, gcFloor: body.gcFloor ?? 0,
                      rows: body.rows, presence: body.presence ?? [:]))
    }

    /// Flush pending op batches over HTTPS while the socket is down (LWW
    /// clocks make replays apply zero ops).
    private func pushPendingOverHTTP() async {
        struct PushBody: Encodable {
            var batch: String
            var ops: [RegistryOp]
        }
        struct AckBody: Decodable {
            var batch: String
            var seq: UInt64
            var applied: UInt64?
        }
        let batches = doc.takePushable()
        guard !batches.isEmpty else { return }
        for pending in batches {
            guard var request = await config.registryPushRequest(),
                  let body = try? JSONEncoder().encode(PushBody(batch: pending.batch,
                                                                ops: pending.ops)) else { break }
            request.httpBody = body
            let result = try? await URLSession.shared.data(for: request)
            let status = (result?.1 as? HTTPURLResponse)?.statusCode
            guard let (data, _) = result, status == 200,
                  let ack = try? JSONDecoder().decode(AckBody.self, from: data) else {
                roomLog.warning("registry: http push failed (http=\(status ?? -1)); will retry")
                // takePushable marked these in flight; nothing clears that
                // until the next socket disconnect — un-mark so the next
                // cycle (HTTP or WS) retries them.
                doc.markDisconnected()
                break
            }
            handle(.ack(batch: ack.batch, seq: ack.seq, applied: ack.applied ?? 0))
        }
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    /// Foreground hook: revive the room after a suspension (see
    /// RegistryClient.kick).
    func kickRoom() {
        if let client {
            Task { await client.kick() }
        }
        restartChangeRequestStreams(resetUnsupported: true)
    }

    func stop() {
        pollTask?.cancel()
        pollTask = nil
        saver?.flush()
        if let client {
            Task { await client.stop() }
        }
        client = nil
        stopChangeRequestStreams()
        let relays = Array(relayClients.values)
        relayClients.removeAll()
        for relay in relays {
            Task { await relay.close() }
        }
        connected = false
    }

    // MARK: Server events (delivered in frame order — rows before ack)

    private func handle(_ event: RegistryEvent) {
        switch event {
        case .state(let seq, let full, let gcFloor, let rows, let beats):
            // On a state frame with full=true and seq < our cursor (server
            // wiped), applyState keeps local rows and re-seeds them as
            // upserts carrying their ORIGINAL per-field clocks; the client
            // pushes those pending batches right after this returns.
            let outcome = doc.applyState(seq: seq, full: full, gcFloor: gcFloor, rows: rows)
            if outcome == .reseeded {
                roomLog.info("registry: server behind local state; re-seeding")
            }
            let now = nowMs()
            for (device, at) in beats {
                presence[device] = at
                presenceReceivedAt[device] = now
            }
            project()
            saver?.poke()
            synced = true
        case .connected:
            let reconnected = !connected
            connected = true
            registryJoinedAt = nowMs()
            if reconnected {
                restartChangeRequestStreams(resetUnsupported: true)
            }
        case .rows(let seq, let rows):
            // A false return = seq gap (missed frames): the rows applied but
            // the cursor held, so the next reconnect's hello backfills the
            // window instead of skipping it forever. iOS sockets churn on
            // every suspension, so convergence is quick without forcing one.
            _ = doc.applyRows(seq: seq, rows: rows)
            project()
            saver?.poke()
        case .ack(let batch, let seq, _):
            // Rows for this batch already arrived (server orders rows before
            // ack), so retiring the optimistic overlay can't flicker — and if
            // our op lost LWW, the merged row is now the truth on display.
            doc.ackBatch(batch, seq: seq)
            project()
            saver?.poke()
        case .presence(let device, let at):
            presence[device] = at
            presenceReceivedAt[device] = nowMs()
        case .disconnected:
            connected = false
            registryJoinedAt = nil
            doc.markDisconnected()
        }
    }

    /// Every local write: re-project the overlay, schedule the snapshot, and
    /// wake the client to push the fresh batch.
    private func afterLocalWrite() {
        project()
        saver?.poke()
        if let client {
            Task { await client.nudge() }
        }
        // Socket down: the write still leaves the device promptly over
        // HTTPS instead of waiting out the reconnect backoff.
        if !connected {
            Task { await self.pushPendingOverHTTP() }
        }
    }

    // MARK: Presence

    func deviceOnline(_ deviceId: String) -> Bool {
        guard let received = presenceReceivedAt[deviceId] else { return false }
        return nowMs() - received < Self.presenceTtlMs
    }

    /// Dial-gate verdict (workspace_host.rs peer_liveness): `dark` requires
    /// POSITIVE evidence of absence — a joined, warmed-up registry room and a
    /// freshest heartbeat older than 5 minutes. Registry-down and every
    /// ambiguity stay `unknown`, so a rows-down/relay-up incident shape can
    /// never park the relay.
    func peerLiveness(_ deviceId: String) -> PeerLiveness {
        guard connected, let joinedAt = registryJoinedAt else { return .unknown }
        let now = nowMs()
        if let received = presenceReceivedAt[deviceId] {
            if now - received < Self.presenceLiveFreshMs { return .live }
            if now - received >= Self.dialGateDarkMs { return .dark }
            return .unknown
        }
        // Never heard this session: only a warmed-up room may consult the
        // durable device row's own stamp.
        guard now - joinedAt >= Self.dialGateWarmupMs else { return .unknown }
        if let seen = devices.first(where: { $0.id == deviceId })?.lastSeenAt,
           now - seen >= Self.dialGateDarkMs {
            return .dark
        }
        return .unknown
    }

    /// Version gates (state.rs device_version_at_least): unknown device or an
    /// unparsable/unstamped version reads as "too old" → legacy behavior.
    func deviceVersionAtLeast(_ deviceId: String, _ min: (Int, Int, Int)) -> Bool {
        guard let raw = devices.first(where: { $0.id == deviceId })?.version,
              let v = versionTriple(raw) else { return false }
        return v >= min
    }

    // MARK: Projection (rows → typed entities)

    private func project() {
        devices = doc.overlayRows(kind: "devices").map { row in
            let f = row.fields
            let id = f["id"]?.stringValue ?? row.id
            return DeviceRow(id: id,
                             name: f["name"]?.stringValue ?? id,
                             platform: f["platform"]?.stringValue ?? "",
                             lastSeenAt: f["lastSeenAt"]?.int64Value,
                             createdAt: f["createdAt"]?.int64Value,
                             version: f["version"]?.stringValue)
        }.sorted { $0.name < $1.name }

        spaces = doc.overlayRows(kind: "spaces").compactMap { row in
            let f = row.fields
            guard let deviceId = f["deviceId"]?.stringValue,
                  let path = f["path"]?.stringValue else { return nil }
            return Space(id: f["id"]?.stringValue ?? row.id, deviceId: deviceId, path: path,
                         name: f["name"]?.stringValue,
                         gitDetected: f["gitDetected"]?.boolValue ?? false,
                         gitCheckedAt: f["gitCheckedAt"]?.int64Value,
                         checkoutId: f["checkoutId"]?.stringValue,
                         createdAt: f["createdAt"]?.int64Value ?? 0)
        }.sorted { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }  // creation order, id tiebreak

        chats = doc.overlayRows(kind: "chats").compactMap { row in
            let f = row.fields
            guard let deviceId = f["deviceId"]?.stringValue else { return nil }
            var chatConfig: ChatConfig?
            if let c = f["config"]?.objectValue {
                chatConfig = ChatConfig(harness: c["harness"]?.stringValue ?? "claude-code",
                                        model: c["model"]?.stringValue,
                                        reasoning: c["reasoning"]?.stringValue,
                                        modelOptions: c["modelOptions"]?.objectValue ?? [:],
                                        sandbox: c["sandbox"]?.stringValue)
            }
            return Chat(id: f["id"]?.stringValue ?? row.id, deviceId: deviceId,
                        title: f["title"]?.stringValue,
                        archived: f["archived"]?.boolValue ?? false,
                        cwd: f["cwd"]?.stringValue,
                        branch: f["branch"]?.stringValue,
                        checkoutId: f["checkoutId"]?.stringValue,
                        config: chatConfig,
                        lastMessagePreview: f["lastMessagePreview"]?.stringValue,
                        lastMessageAt: f["lastMessageAt"]?.int64Value,
                        createdAt: f["createdAt"]?.int64Value ?? 0,
                        spaceId: f["spaceId"]?.stringValue,
                        lastSeenAt: f["lastSeenAt"]?.int64Value,
                        roomGen: f["roomGen"]?.int64Value.map(Int.init))
        }

        var rows: [String: SessionRow] = [:]
        for row in doc.overlayRows(kind: "sessions") {
            let f = row.fields
            guard let chatId = f["chatId"]?.stringValue,
                  let deviceId = f["deviceId"]?.stringValue,
                  let statusStr = f["status"]?.stringValue,
                  let status = SessionStatus(rawValue: statusStr) else { continue }
            rows[chatId] = SessionRow(chatId: chatId, deviceId: deviceId, status: status,
                                      startedAt: f["startedAt"]?.int64Value,
                                      updatedAt: f["updatedAt"]?.int64Value ?? 0)
        }
        sessions = rows
        reconcileChangeRequestStreams()
    }

    // MARK: Derived views

    /// state.rs `overview_chats`: every non-archived chat of a live space,
    /// attention-sorted.
    var overviewChats: [Chat] {
        let liveSpaceIds = Set(spaces.map(\.id))
        let live = chats.filter { !$0.archived && $0.spaceId.map(liveSpaceIds.contains) == true }
        return sortActive(live)
    }

    /// A space's sessions, in the sidebar's Sessions order (recency).
    ///
    /// NOT desktop's `chats_in_space`, which is creation order because there
    /// the rows are TABS and activity must never reorder tabs. The phone has
    /// no tabs — a space opens into the same list, with the same rows, as the
    /// Sessions section — so it follows that list's ordering instead.
    func chats(in spaceId: String) -> [Chat] {
        sortActive(chats.filter { !$0.archived && $0.spaceId == spaceId })
    }

    /// Archived chats under an optional space scope, recency order — feeds the
    /// Archived shelf (shell/spaces.rs `render_archived_section`). Unlike
    /// `overviewChats`, a live space is not required: an archived session of a
    /// deleted space should still be reachable for unarchive.
    func archivedChats(in spaceId: String? = nil) -> [Chat] {
        sortActive(chats.filter { $0.archived && (spaceId == nil || $0.spaceId == spaceId) })
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        chatIndicator(chat: chat, live: effectiveStatus(sessions[chat.id], now: nowMs()))
    }

    func changeRequest(for chat: Chat) -> ChangeRequestSummary? {
        resolvedChangeRequest(for: chat, spaces: spaces, snapshots: changeRequestSnapshots)
    }

    /// Aggregate most-urgent member status for a space's leading dot.
    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        let members = chats(in: spaceId).map { indicator(for: $0) }
        return members.min(by: { $0.rawValue < $1.rawValue })
    }

    // MARK: Device relay (folder browsing / direct host RPCs)

    @ObservationIgnored private var relayClients: [String: DeviceRelayClient] = [:]
    @ObservationIgnored private var changeRequestTasks: [ChangeRequestWatchKey: Task<Void, Never>] = [:]
    @ObservationIgnored private var unsupportedChangeRequestDevices: Set<String> = []

    private func relay(for deviceId: String) -> DeviceRelayClient {
        if let existing = relayClients[deviceId] { return existing }
        let client = DeviceRelayClient(deviceId: deviceId, config: config,
                                       liveness: { [weak self] in
                                           self?.peerLiveness(deviceId) ?? .unknown
                                       })
        relayClients[deviceId] = client
        return client
    }

    /// Chunked upload straight to a device (the legacy host-staged path for
    /// hosts that predate queued attachments) — used by the new-session
    /// canvas, where no SessionStore exists yet.
    func uploadAttachment(deviceId: String, name: String, data: Data,
                          uploadId: String,
                          progress: (@MainActor @Sendable (Double) -> Void)? = nil) async throws -> String {
        try await uploadAttachmentChunked(relay: relay(for: deviceId), name: name,
                                          data: data, uploadId: uploadId,
                                          progress: progress)
    }

    private func reconcileChangeRequestStreams() {
        let targets = desiredChangeRequestTargets(
            chats: chats,
            spaces: spaces,
            unsupportedDevices: unsupportedChangeRequestDevices
        )

        for target in Array(changeRequestTasks.keys) where !targets.contains(target) {
            changeRequestTasks.removeValue(forKey: target)?.cancel()
        }
        changeRequestSnapshots = changeRequestSnapshots.filter { targets.contains($0.key) }

        for target in targets where changeRequestTasks[target] == nil {
            let client = relay(for: target.deviceId)
            changeRequestTasks[target] = Task { [weak self] in
                guard let self else { return }
                await self.watchChangeRequest(target, through: client)
            }
        }
    }

    private func watchChangeRequest(
        _ target: ChangeRequestWatchKey,
        through client: DeviceRelayClient
    ) async {
        var retryNanoseconds: UInt64 = 500_000_000
        while !Task.isCancelled {
            do {
                let stream: AsyncThrowingStream<CheckoutChangeRequestStatus, Error> = try await client.stream(
                    method: "WatchCheckoutChangeRequest",
                    params: ["cwd": target.cwd]
                )
                for try await snapshot in stream {
                    guard !Task.isCancelled else { return }
                    // Never expose a misrouted frame under another host/path.
                    guard snapshot.deviceId == target.deviceId,
                          snapshot.cwd == target.cwd else { continue }
                    changeRequestSnapshots[target] = snapshot
                    retryNanoseconds = 500_000_000
                }
            } catch {
                guard !Task.isCancelled else { return }
                if isUnknownChangeRequestMethod(error) {
                    markChangeRequestsUnsupported(on: target.deviceId)
                    return
                }
            }

            // Preserve the latest successful snapshot through transport gaps.
            try? await Task.sleep(nanoseconds: retryNanoseconds)
            retryNanoseconds = min(retryNanoseconds * 2, 5_000_000_000)
        }
    }

    private func markChangeRequestsUnsupported(on deviceId: String) {
        unsupportedChangeRequestDevices.insert(deviceId)
        for target in Array(changeRequestTasks.keys) where target.deviceId == deviceId {
            changeRequestTasks.removeValue(forKey: target)?.cancel()
        }
        changeRequestSnapshots = changeRequestSnapshots.filter { $0.key.deviceId != deviceId }
    }

    private func restartChangeRequestStreams(resetUnsupported: Bool) {
        for task in changeRequestTasks.values { task.cancel() }
        changeRequestTasks.removeAll()
        if resetUnsupported { unsupportedChangeRequestDevices.removeAll() }
        reconcileChangeRequestStreams()
    }

    private func stopChangeRequestStreams() {
        for task in changeRequestTasks.values { task.cancel() }
        changeRequestTasks.removeAll()
        unsupportedChangeRequestDevices.removeAll()
        changeRequestSnapshots.removeAll()
    }

    /// The last relay failure, for surfacing in UI/diagnostics.
    private(set) var lastRelayError: String?

    /// ListFolders on the target device (engine caps at 500 entries, hides
    /// dotfiles, stamps isRepo). nil path = the device's home directory.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        do {
            return try await listFoldersDetailed(deviceId: deviceId, path: path)
        } catch {
            lastRelayError = error.localizedDescription
            return nil
        }
    }

    func listFoldersDetailed(deviceId: String, path: String?) async throws -> FolderListing {
        var params: [String: Any] = [:]
        if let path { params["path"] = path }
        return try await relay(for: deviceId).call(method: "ListFolders", params: params)
    }

    /// ListRefs on the target device — branches with current/worktree markers
    /// (default branch first, per the engine's ordering).
    func listRefs(deviceId: String, repoPath: String) async -> [RepoRef]? {
        try? await relay(for: deviceId).call(method: "ListRefs", params: ["repoPath": repoPath])
    }

    /// ListModels — the target device's live harness catalog (the desktop
    /// discovers models from the CLI itself; static lists are only fallback).
    /// The device's harness catalog (`ListHarnesses` → `[HarnessDescriptor]`),
    /// filtered to what the composer may offer: installed AND enabled (the
    /// Settings → Agents gate; absent `enabled` falls back to the engine's
    /// `default_enabled()` pair, matching `descriptor_enabled`).
    func listHarnesses(deviceId: String) async -> [HarnessInfo]? {
        struct WireHarness: Decodable {
            var id: String
            var name: String
            var installed: Bool?
            var enabled: Bool?
        }
        let wire: [WireHarness]? = try? await relay(for: deviceId)
            .call(method: "ListHarnesses", params: [:])
        return wire.map { list in
            list.filter { h in
                h.id != "mock"
                    && (h.installed ?? true)
                    && (h.enabled ?? ["claude-code", "codex"].contains(h.id))
            }
            .map { HarnessInfo(id: $0.id, label: $0.name) }
        }
    }

    func listModels(deviceId: String, harness: String) async -> [ModelInfo]? {
        struct WireChoice: Decodable {
            var id: String
            var label: String
        }
        struct WireOption: Decodable {
            var id: String
            var label: String
            var choices: [WireChoice]
            var defaultChoice: String
        }
        struct WireModel: Decodable {
            var id: String
            var label: String
            var description: String?
            var reasoningLevels: [String]?
            var options: [WireOption]?
        }
        let wire: [WireModel]? = try? await relay(for: deviceId)
            .call(method: "ListModels", params: ["harness": harness])
        return wire.map { models in
            models.map {
                ModelInfo(id: $0.id, label: $0.label, description: $0.description,
                          reasoningLevels: $0.reasoningLevels ?? [],
                          options: ($0.options ?? []).map { option in
                              ModelOptionInfo(
                                  id: option.id,
                                  label: option.label,
                                  choices: option.choices.map {
                                      ModelOptionChoiceInfo(id: $0.id, label: $0.label)
                                  },
                                  defaultChoice: option.defaultChoice
                              )
                          })
            }
        }
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
    /// setChatCwd/setChatBranch mutates — LWW field writes here).
    func setChatCheckout(chatId: String, cwd: String, branch: String) {
        updateChat(chatId, set: ["cwd": .string(cwd), "branch": .string(branch)])
    }

    // MARK: Writes (viewer-device discipline → op batches)

    /// Mint a new chat onto a space (workspace_host.rs create_chat shape):
    /// a full-row upsert. The host = the space's owning device picks it up
    /// via the registry.
    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) -> String {
        let chatId = UUID().uuidString.lowercased()
        var set: [String: JSONValue] = [
            "id": .string(chatId),
            "deviceId": .string(space.deviceId),
            "archived": .bool(false),
            "cwd": .string(cwd ?? space.path),
            "spaceId": .string(space.id),
            "createdAt": .int(nowMs()),
            // Born on chat2 (workspace_host.rs create_chat): a brand-new
            // chat has an empty doc — nothing to seed, no migration race.
            "roomGen": .int(2),
        ]
        if let branch {
            set["branch"] = .string(branch)
        }
        if let cfg = JSONValue(encodable: chatConfig) {
            set["config"] = cfg
        }
        doc.write(kind: "chats", id: chatId, op: .upsert, set: set)
        afterLocalWrite()
        return chatId
    }

    /// Create a space. Preferred path: `Mutate {op:createSpace}` straight to
    /// the owning host over its relay (it applies the row to its own registry
    /// doc, functionally identical to the desktop's local mutate + sync).
    /// Fallback when the host is unreachable: a full-row upsert from here —
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
            doc.write(kind: "spaces", id: spaceId, op: .upsert, set: [
                "id": .string(spaceId),
                "deviceId": .string(deviceId),
                "path": .string(path),
                "gitDetected": .bool(gitDetected),
                "createdAt": .int(nowMs()),
            ])
        }
        afterLocalWrite()
        return spaceId
    }

    func setArchived(chatId: String, archived: Bool) {
        updateChat(chatId, set: ["archived": .bool(archived)])
    }

    /// Synced seen marker (LWW) with a monotonic guard: no write when the
    /// stored stamp is already current.
    func markSeen(chatId: String) {
        guard let row = doc.overlayRow(kind: "chats", id: chatId) else { return }
        let at = nowMs()
        if let current = row.fields["lastSeenAt"]?.int64Value, current >= at { return }
        doc.write(kind: "chats", id: chatId, op: .update, set: ["lastSeenAt": .int(at)])
        afterLocalWrite()
    }

    func rename(chatId: String, title: String) {
        updateChat(chatId, set: ["title": .string(title)])
    }

    /// Chat config is an LWW field on the chat row; the host reads it when
    /// dispatching the next run.
    func setChatConfig(chatId: String, config chatConfig: ChatConfig) {
        guard let value = JSONValue(encodable: chatConfig) else { return }
        updateChat(chatId, set: ["config": value])
    }

    /// Tombstone a chat (and its session-status row) in one batch. The
    /// per-chat session doc remains — this removes the index entry only.
    func deleteChat(chatId: String) {
        doc.deleteRows([("chats", chatId), ("sessions", chatId)])
        afterLocalWrite()
    }

    /// Hard-delete a space and cascade to its chats: ONE batch tombstones the
    /// space row and every chat/session row whose spaceId matches — the
    /// server applies the batch atomically.
    func deleteSpace(spaceId: String) {
        var keys: [(kind: String, id: String)] = []
        for chat in chats where chat.spaceId == spaceId {
            keys.append(("chats", chat.id))
            keys.append(("sessions", chat.id))
        }
        keys.append(("spaces", spaceId))
        doc.deleteRows(keys)
        afterLocalWrite()
    }

    /// Field sets are `update` ops — they NEVER create or revive rows (the
    /// old "never invent rows" discipline), so check the overlay row first.
    private func updateChat(_ chatId: String, set: [String: JSONValue]) {
        guard doc.rowExists(kind: "chats", id: chatId) else { return }
        doc.write(kind: "chats", id: chatId, op: .update, set: set)
        afterLocalWrite()
    }
}
