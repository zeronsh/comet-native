// Session doc mirror — transcript entries + the durable command queue for one
// chat (crates/doc/src/schema.rs), synced over the chat2 log relay
// (docs/chat2-sync.md; s2 is dead on mobile). A viewer device never writes
// message entries; it appends command ledger entries (rule 1) and lets the
// host drain them. Optimistic echo: pending sends render locally under their
// client-minted message id until the host writes the real entry with the
// same id.
//
// The registry names the room generation (M2): the store connects only once
// the chat row says roomGen 2. A gen-1 chat renders nothing and waits for
// the host's migration sweep to flip it — the local doc is always the chat2
// lineage; a cached pre-chat2 snapshot is never imported (unrelated Loro
// histories would duplicate every message), only mined for our own pending
// commands (M3) and left on disk as rollback.

import Foundation
import Loro
import Observation

/// One queued-flow attachment: client-minted uploadId (the pending:// ref's
/// identity), original file name, and the bytes the escort pushes.
struct AttachmentTransfer {
    let uploadId: String
    let name: String
    let data: Data
}

/// An optimistic echo the host hasn't materialized yet. `at` is the send's
/// wall time (display); `started` is the delivery-grace clock, reset by the
/// retry affordance so the surface returns to Sending/Queued.
struct PendingSend {
    let messageId: String
    let text: String
    let at: Int64
    var started: Int64
}

@MainActor
@Observable
final class SessionStore {
    let chatId: String
    /// The chat's host device — nudge target for cold-host command drains.
    var hostDeviceId: String?
    private(set) var entries: [MessageEntry] = []
    /// Bumped on every change to `entries` / `pendingSends`. The transcript's
    /// row builder memoizes on it, so a body re-eval that was triggered by
    /// something else (scrolling) costs O(1) instead of re-deriving every row.
    private(set) var revision: UInt64 = 0
    /// Transcript parse/row cache — store-owned so parses survive view
    /// churn, and prewarmed off-main whenever a projection lands so opening
    /// the chat never parses markdown inside the first body pass.
    @ObservationIgnored let transcriptCache = TranscriptBuilderCache()
    private(set) var connected = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [PendingSend] = []
    /// Local submission only; remote user entries never pull a reader to a new turn.
    private(set) var lastSubmittedMessageId: String?
    /// Presentation state survives navigation with the warm session store.
    var expandedUserMessages: Set<String> = []

    let doc = LoroDoc()
    /// The chat2 room cursor — the last server row seq folded into `doc`.
    /// Persisted WITH the snapshot in one atomic file (DocDisk.saveChat2, the
    /// C2 rule), so content and cursor can never diverge.
    @ObservationIgnored private var cursor: UInt64 = 0
    private var chatRoom: ChatRoomClient?
    private var subscriptions: [Subscription] = []
    private let config: AppConfig
    /// Registry roomGen for this chat (M2): connect only at >= 2. One-way —
    /// the registry never walks a chat back to s2.
    @ObservationIgnored private var roomGen = 1
    @ObservationIgnored private var started = false
    /// Preload holds the dial (AppModel staggers the release) so a cold
    /// launch doesn't stampede N TLS handshakes against the registry dial
    /// on a thin link. The disk snapshot still hydrates immediately —
    /// only the socket waits its turn.
    @ObservationIgnored private var holdDial = false

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?

    init(chatId: String, config: AppConfig, offline: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.offline = offline
        AttachmentImageCache.shared.configure(config: config)
    }

    // MARK: Attachments (uploads target the chat's host device)

    @ObservationIgnored private var hostRelay: (deviceId: String, client: DeviceRelayClient)?
    /// Registry-presence dial gate for the host relay, wired by AppModel.
    @ObservationIgnored var hostLiveness: (@MainActor @Sendable (String) -> PeerLiveness)?

    private func relayToHost() throws -> DeviceRelayClient {
        guard let hostDeviceId else { throw RelayError.hostOffline }
        if let hostRelay, hostRelay.deviceId == hostDeviceId {
            return hostRelay.client
        }
        let target = hostDeviceId
        let relay: DeviceRelayClient
        if let gate = hostLiveness {
            relay = DeviceRelayClient(deviceId: target, config: config,
                                      liveness: { gate(target) })
        } else {
            relay = DeviceRelayClient(deviceId: target, config: config)
        }
        hostRelay = (target, relay)
        return relay
    }

    /// Chunked upload of one staged image to the host device (the LEGACY
    /// host-staged flow, for hosts < 0.2.12); returns the durable absolute
    /// path on that device (what the refs trailer carries).
    func uploadAttachment(name: String, data: Data, uploadId: String? = nil,
                          progress: (@MainActor @Sendable (Double) -> Void)? = nil) async throws -> String {
        try await uploadAttachmentChunked(relay: relayToHost(), name: name, data: data,
                                          uploadId: uploadId, progress: progress)
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        entries = new
        revision &+= 1
        transcriptCache.prewarm(entries: entries)
    }

    @ObservationIgnored private var saver: DocSaver?

    func start(holdDial: Bool = false) {
        guard !started, !offline else { return }
        started = true
        self.holdDial = holdDial
        // Local-first: the last-synced chat2 snapshot renders instantly (even
        // when the host device is offline); the join backfills incrementally
        // from its cursor.
        if let saved = DocDisk.loadChat2(into: doc, id: chatId) {
            cursor = saved
            project()
        } else if DocDisk.legacySnapshotExists(id: chatId) {
            // M3 discard-and-adopt: this device's cached doc predates the
            // chat2 lineage. Carry over OUR OWN unresolved commands as fresh
            // entries; the chat2 catch-up repopulates the transcript.
            adoptLegacyCommands()
        }
        saver = DocSaver { [weak self] in
            guard let self else { return }
            DocDisk.saveChat2(doc: self.doc, id: self.chatId, cursor: self.cursor)
        }
        // Subscription BEFORE any connect: every local commit lands in the
        // client when it exists; commits made earlier are covered by the
        // first-contact full-log push below (cursor 0 whenever no client has
        // ever acked — see connectIfReady).
        let localSub = doc.subscribeLocalUpdate { [weak self] update in
            let bytes = Data(update)
            Task { @MainActor [weak self] in
                guard let self else { return }
                if let room = self.chatRoom {
                    Task { await room.enqueue(update: bytes) }
                }
                self.saver?.poke()
            }
        }
        subscriptions.append(localSub)
        connectIfReady()
        project()
        // A relaunch mid-send: re-arm the attachment escorts for any of our
        // still-pending commands whose bytes sit in the stash.
        respawnEscorts()
    }

    /// Registry projection hook (AppModel forwards the chat row's roomGen).
    /// A gen-1 store connects the moment the host's migration sweep flips
    /// the row.
    func updateRoomGen(_ gen: Int?) {
        let gen = gen ?? 1
        if gen > roomGen { roomGen = gen }
        connectIfReady()
    }

    /// Still holding the preload dial (never connected) — the kick sweep
    /// skips these so a foreground/path flap can't stampede held sockets.
    var isDialHeld: Bool { holdDial }

    /// End a preload dial-hold: an open view (or the stagger timer) wants
    /// live sync now.
    func releaseDial() {
        guard holdDial else { return }
        holdDial = false
        connectIfReady()
    }

    private func connectIfReady() {
        guard started, !offline, !holdDial, chatRoom == nil, roomGen >= 2 else { return }
        let delegate = ChatRoomClient.Delegate(
            cursor: { [weak self] in self?.cursor ?? 0 },
            containsFrontier: { [weak self] frontier in
                // Deliberately NO empty-frontier shortcut (mirror of
                // EngineChatSink::contains_frontier): an empty payload on a
                // present checkpoint is unreadable provenance, not proof of
                // emptiness — skipping made fresh readers park every row that
                // depends on the chat's founding ops ("Add Tweets" incident,
                // 2026-08-18). Empty fails the decode: NOT contained, fetch —
                // always safe, never silently skips history.
                guard let self, !frontier.isEmpty,
                      let vv = try? VersionVector.decode(bytes: frontier) else { return false }
                // A decoded-but-EMPTY version vector is a vacuous claim every
                // doc "includes" — the actual poison, one representation
                // deeper than zero-length bytes. Fetch.
                guard !vv.toHashmap().isEmpty else {
                    roomLog.info("chat2 \(self.chatId, privacy: .public): frontier decodes empty (vacuous); fetching checkpoint")
                    return false
                }
                return self.doc.oplogVv().includesVv(other: vv)
            },
            applyCheckpoint: { [weak self] bytes, seq in
                guard let self,
                      (try? self.doc.importWith(bytes: bytes, origin: "remote")) != nil else {
                    return false
                }
                self.cursor = max(self.cursor, seq)
                self.project()
                self.saver?.poke()
                return true
            },
            applyRow: { [weak self] bytes, seq in
                guard let self else { return }
                // Malformed remote bytes cost the row, never the doc. The
                // cursor still advances: replaying a poison row forever is
                // the wedge class chat2 replaces.
                if (try? self.doc.importWith(bytes: bytes, origin: "remote")) == nil {
                    roomLog.warning("chat2 \(self.chatId, privacy: .public): row import failed; skipping row \(seq)")
                }
                self.cursor = max(self.cursor, seq)
                self.project()
                self.saver?.poke()
            },
            advanceCursor: { [weak self] seq in
                guard let self else { return }
                self.cursor = max(self.cursor, seq)
                self.saver?.poke()
            },
            clampCursor: { [weak self] seq in
                guard let self, self.cursor > seq else { return }
                // Cursor amnesty (see ChatRoomClient): a cursor above the
                // room's checkpoint is only as trustworthy as the doc under
                // it — rows imported while their deps were missing PARK
                // silently, vanish on export, and the cursor lied forever
                // ("Add Tweets" wedge: cursor 75 over a checkpoint-only doc,
                // 2026-08-18). Clamping re-fetches rows since the checkpoint
                // (KB-bounded by trim policy; re-imports are no-ops), which
                // converts any lying cursor into a true one.
                roomLog.info("chat2 \(self.chatId, privacy: .public): cursor amnesty \(self.cursor) → \(seq)")
                self.cursor = seq
                self.saver?.poke()
            },
            setCursor: { [weak self] seq in
                guard let self, self.cursor != seq else { return }
                self.cursor = seq
                self.saver?.poke()
            },
            event: { [weak self] event in self?.handle(event) }
        )
        let client = ChatRoomClient(
            chatId: chatId, device: config.deviceId,
            urlProvider: { [config, chatId] in await config.chat2SocketURL(chatId: chatId) },
            checkpointRequest: { [config, chatId] in
                await config.chat2CheckpointRequest(chatId: chatId)
            },
            rowsRequest: { [config, chatId] after in
                await config.chat2RowsRequest(chatId: chatId, after: after)
            },
            pushRequest: { [config, chatId] batchId in
                await config.chat2PushRequest(chatId: chatId, batchId: batchId)
            },
            delegate: delegate)
        chatRoom = client
        // First contact with the room (cursor 0): everything committed
        // BEFORE the local-update subscription saw a client — an adopt's
        // requeued commands, sends queued while waiting for the roomGen
        // flip — is invisible to the push path, yet every later commit
        // causally depends on it (doc_host.rs first-contact rule; rows built
        // on unpushed deps sit in peers' pending-dep buffers forever). Push
        // the doc's full update log as the join's first batch; once acked
        // the cursor moves and this never re-arms.
        if cursor == 0,
           let all = try? doc.export(mode: .updates(from: VersionVector())), !all.isEmpty {
            Task { await client.enqueue(update: all) }
        }
        Task { await client.start() }
    }

    /// Mine the retired s2 snapshot for OUR OWN still-pending commands and
    /// re-queue them into the fresh lineage (doc_host.rs M3 requeue: same
    /// command ids — the host's processed_commands ledger guards double
    /// execution; basedOn is dropped, its turn ids don't exist here).
    private func adoptLegacyCommands() {
        let legacy = LoroDoc()
        guard DocDisk.load(into: legacy, id: chatId),
              let root = legacy.getDeepValue().mapValue,
              let commands = root["commands"]?.listValue, !commands.isEmpty else { return }
        let now = nowMs()
        var carried = 0
        let fresh = doc.getList(id: "commands")
        for value in commands {
            guard let m = value.mapValue,
                  m["status"]?.stringValue == "pending",
                  m["issuedBy"]?.stringValue == config.deviceId,
                  let id = m["id"]?.stringValue,
                  let kind = m["kind"]?.stringValue,
                  let payload = m["payload"] else { continue }
            if let expires = m["expiresAt"]?.i64Value, expires <= now { continue }
            do {
                let map = try fresh.pushContainer(child: LoroMap())
                try map.insert(key: "id", v: id)
                try map.insert(key: "kind", v: kind)
                try map.insert(key: "payload", v: payload)
                try map.insert(key: "issuedBy", v: config.deviceId)
                try map.insert(key: "issuedAt", v: m["issuedAt"]?.i64Value ?? now)
                try map.insert(key: "expiresAt", v: m["expiresAt"]?.i64Value ?? (now + commandDefaultTtlMs))
                try map.insert(key: "status", v: "pending")
                carried += 1
            } catch {}
        }
        guard carried > 0 else { return }
        doc.commit()
        roomLog.info("chat2 \(self.chatId, privacy: .public): adopt carried \(carried) pending command(s) from the s2 lineage")
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    /// Foreground hook: revive the room after a suspension (see
    /// ChatRoomClient.kick). Also the catch-all re-check for a roomGen flip
    /// that landed while this store had no open view.
    func kickRoom() {
        holdDial = false  // a kick is a user/foreground signal: dial now
        connectIfReady()
        guard let chatRoom else { return }
        Task { await chatRoom.kick() }
    }

    func stop() {
        subscriptions.removeAll()
        saver?.flush()
        if let chatRoom {
            Task { await chatRoom.stop() }
        }
        chatRoom = nil
        connected = false
    }

    private func handle(_ event: ChatRoomEvent) {
        switch event {
        case .connected:
            connected = true
            project()
        case .disconnected:
            connected = false
        }
    }

    // MARK: Projection

    /// In-flight guard + trailing re-run for the off-main projection below.
    @ObservationIgnored private var projecting = false
    @ObservationIgnored private var projectPending = false

    /// Re-derive `entries` from the doc, off the main thread.
    ///
    /// `getDeepValue()` materializes the WHOLE doc and the decode walks every
    /// message and every part, so this is O(transcript) — tens of ms on a big
    /// session, and it runs on every remote update. On the main actor that
    /// stalled the first frame of a cached session and janked streaming.
    /// Reading the doc from a background task is the access class the design
    /// already has: `ChatRoomClient` applies through main-actor closures, but
    /// the doc remains concurrently readable today regardless.
    ///
    /// Overlapping calls coalesce to a single trailing re-run — a streaming
    /// burst must not queue one whole-doc projection per token.
    private func project() {
        guard !projecting else {
            projectPending = true
            return
        }
        projecting = true
        let doc = self.doc
        Task { @MainActor [weak self] in
            let decoded = await Task.detached(priority: .userInitiated) {
                Self.decodeEntries(from: doc)
            }.value
            guard let self else { return }
            self.projecting = false
            if let decoded {
                self.apply(decoded)
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: [MessageEntry]) {
        entries = decoded
        // Drop echoes the host has materialized.
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
        revision &+= 1
        // If no transcript view is open, settle the parses now (off-main) so
        // the eventual open is memo hits all the way down.
        transcriptCache.prewarm(entries: entries)
    }

    /// Whole-doc decode. `nil` means the doc has no map root yet — leave the
    /// previous projection standing rather than blanking a live transcript.
    nonisolated static func decodeEntries(from doc: LoroDoc) -> [MessageEntry]? {
        guard let root = doc.getDeepValue().mapValue else { return nil }
        let raw = (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        return joinContinuations(raw)
    }

    nonisolated private static func entryFrom(_ value: LoroValue) -> MessageEntry? {
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

    nonisolated private static func partFrom(_ value: LoroValue) -> MessagePart? {
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
    nonisolated static func joinContinuations(_ raw: [MessageEntry]) -> [MessageEntry] {
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
                // An empty question list can't be answered, so it must not take
                // the composer's place — leaving the user with no way to type.
                if case .input(_, let requestId, let questions, let resolved) = part,
                   !resolved, !questions.isEmpty {
                    return (entry.id, requestId, questions)
                }
            }
        }
        return nil
    }

    // MARK: Command plane (ledger rule 1: append-only, own entries only)

    func sendRun(prompt: String, chat: Chat, attachments: [String] = [],
                 worktree: WorktreeSpec? = nil) {
        if offline {
            demoResponder?(prompt)
            lastSubmittedMessageId = entries.last(where: { $0.role == .user })?.id
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let request = RunRequest(prompt: prompt,
                                 harness: chat.config?.harness,
                                 model: chat.config?.model,
                                 reasoning: chat.config?.reasoning,
                                 modelOptions: chat.config?.modelOptions ?? [:],
                                 cwd: chat.cwd ?? "",
                                 sandbox: chat.config?.sandbox ?? "workspace-write",
                                 attachments: attachments,
                                 worktree: worktree)
        queueCommand(kind: "run", payload: [
            "kind": "run",
            "request": encodableJSON(request),
            "messageId": messageId,
        ])
        let now = nowMs()
        pendingSends.append(PendingSend(messageId: messageId, text: prompt, at: now, started: now))
        lastSubmittedMessageId = messageId
        revision &+= 1
    }

    func sendSteer(prompt: String) {
        if offline {
            demoResponder?(prompt)
            lastSubmittedMessageId = entries.last(where: { $0.role == .user })?.id
            return
        }
        let messageId = UUID().uuidString.lowercased()
        queueCommand(kind: "steer", payload: [
            "kind": "steer",
            "prompt": prompt,
            "messageId": messageId,
        ])
        let now = nowMs()
        pendingSends.append(PendingSend(messageId: messageId, text: prompt, at: now, started: now))
        lastSubmittedMessageId = messageId
        revision &+= 1
    }

    /// The queued-attachment send (PR #168, host ≥ 0.2.12): the command
    /// queues IMMEDIATELY with `pending://{uploadId}/{name}` refs — a durable
    /// local write — and the bytes chase it over the relay (retry-forever on
    /// the online bus). The host defers the command until every ref's bytes
    /// land, then rewrites the refs to absolute paths at dispatch. An image
    /// send no longer dies with a dead link.
    func sendWithTransfers(prompt: String, chat: Chat, live: Bool,
                           transfers: [AttachmentTransfer],
                           worktree: WorktreeSpec? = nil) {
        // Stash bytes FIRST — before anything references them — so escorts
        // survive a relaunch and retries can re-derive their transfers.
        for transfer in transfers {
            UploadStash.save(uploadId: transfer.uploadId, data: transfer.data)
        }
        let refs = transfers.map { UploadStash.pendingRef(uploadId: $0.uploadId, name: $0.name) }
        let content = withAttachments(text: prompt, paths: refs)
        if live {
            sendSteer(prompt: content)
        } else {
            sendRun(prompt: content, chat: chat, attachments: refs, worktree: worktree)
        }
        spawnEscort(transfers: transfers)
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

    // MARK: Delivery escorts (doc_host.rs spawn_command_delivery, phone half)

    /// doc_host.rs TRANSFER_BACKOFF_BASE / TRANSFER_BACKOFF_CAP /
    /// ATTACHMENT_WAIT_MAX: the escort retries until the bytes land or the
    /// host's own 15-minute defer window closes.
    private static let transferBackoffBaseMs = 2_000
    private static let transferBackoffCapMs = 30_000
    private static let attachmentWaitMaxMs: Int64 = 15 * 60_000

    /// uploadIds an escort is actively pushing — retry/respawn dedupes on it.
    @ObservationIgnored private var activeEscorts: Set<String> = []
    /// Fraction of the current escort batch's bytes committed to the host
    /// (PR #185's "the ring tracks the real relay transfer" — the status
    /// strip narrates it as "Uploading… N%"). nil = no transfer in flight.
    private(set) var transferProgress: Double?

    /// Whether this store has ever dialed its chat2 room (feeds the
    /// connectivity center — an undialed room is not "degraded").
    var roomActive: Bool { chatRoom != nil }

    /// Push a queued send's bytes to the host: retry-forever (event-driven
    /// backoff, cut short by online events) up to the host's defer window.
    /// Success commits every upload and nudges the drain; the command stays
    /// durably queued in the doc no matter what happens here.
    private func spawnEscort(transfers: [AttachmentTransfer]) {
        let remaining = transfers.filter { !activeEscorts.contains($0.uploadId) }
        guard !remaining.isEmpty else { return }
        for transfer in remaining {
            activeEscorts.insert(transfer.uploadId)
        }
        Task { @MainActor [weak self] in
            defer {
                for transfer in remaining {
                    self?.activeEscorts.remove(transfer.uploadId)
                }
                self?.transferProgress = nil
            }
            var pending = remaining
            var backoffMs = Self.transferBackoffBaseMs
            let deadline = nowMs() + Self.attachmentWaitMaxMs
            let totalBytes = max(remaining.reduce(0) { $0 + $1.data.count }, 1)
            while let self, !pending.isEmpty, nowMs() < deadline {
                do {
                    while let transfer = pending.first {
                        let doneBytes = totalBytes - pending.reduce(0) { $0 + $1.data.count }
                        _ = try await uploadAttachmentChunked(
                            relay: self.relayToHost(),
                            name: transfer.name, data: transfer.data,
                            uploadId: transfer.uploadId) { [weak self] fraction in
                            self?.transferProgress = min(
                                (Double(doneBytes) + fraction * Double(transfer.data.count))
                                    / Double(totalBytes), 0.99)
                        }
                        UploadStash.delete(uploadId: transfer.uploadId)
                        pending.removeFirst()
                    }
                    self.nudgeHost()
                    return
                } catch {
                    roomLog.warning("chat2 \(self.chatId, privacy: .public): attachment transfer failed (\(error.localizedDescription, privacy: .public)); retrying in \(backoffMs)ms")
                    await OnlineBus.shared.waitBackoff(ms: backoffMs)
                    backoffMs = min(backoffMs * 2, Self.transferBackoffCapMs)
                }
            }
            if !pending.isEmpty {
                roomLog.error("chat2 \(self?.chatId ?? "?", privacy: .public): attachment transfer gave up after 15min; the send stays queued — retry re-derives the transfers")
            }
        }
    }

    /// Re-derive escorts from the doc's own pending commands (retry taps,
    /// app relaunch): scan our unexpired pending entries for pending:// refs
    /// and re-push any whose bytes are still stashed. Idempotent — a re-push
    /// re-commits the same file; the host's processed ledger keeps execution
    /// exactly-once.
    private func respawnEscorts() {
        guard let root = doc.getDeepValue().mapValue,
              let commands = root["commands"]?.listValue, !commands.isEmpty else { return }
        let now = nowMs()
        var transfers: [AttachmentTransfer] = []
        var seen = Set<String>()
        for value in commands {
            guard let m = value.mapValue,
                  m["status"]?.stringValue == "pending",
                  m["issuedBy"]?.stringValue == config.deviceId,
                  let payload = m["payload"]?.mapValue else { continue }
            if let expires = m["expiresAt"]?.i64Value, expires <= now { continue }
            var refs: [String] = []
            if let request = payload["request"]?.mapValue,
               let attachments = request["attachments"]?.listValue {
                refs += attachments.compactMap(\.stringValue)
            }
            if let prompt = payload["prompt"]?.stringValue {
                refs += prompt.split(separator: "\n").compactMap { line in
                    let trimmed = line.trimmingCharacters(in: .whitespaces)
                    guard trimmed.hasPrefix("- \(UploadStash.pendingRefPrefix)") else { return nil }
                    return String(trimmed.dropFirst(2))
                }
            }
            for ref in refs {
                guard let (uploadId, name) = UploadStash.parseRef(ref),
                      seen.insert(uploadId).inserted,
                      let data = UploadStash.load(uploadId: uploadId) else { continue }
                transfers.append(AttachmentTransfer(uploadId: uploadId, name: name, data: data))
            }
        }
        guard !transfers.isEmpty else { return }
        roomLog.info("chat2 \(self.chatId, privacy: .public): respawning \(transfers.count) attachment escort(s)")
        spawnEscort(transfers: transfers)
    }

    /// The "Not delivered — tap to retry" affordance (doc_host.rs
    /// retry_delivery): restart the grace clock so the surface returns to
    /// Sending/Queued, re-issue dead attempts, kick the room on fresh
    /// backoff, nudge the host, and re-derive attachment escorts from the
    /// still-pending refs (a re-issued command's refs are included).
    func retryDelivery() {
        let now = nowMs()
        for ix in pendingSends.indices {
            pendingSends[ix].started = now
        }
        revision &+= 1
        reissueDeadSends()
        kickRoom()
        nudgeHost()
        respawnEscorts()
    }

    /// PR #172's retry semantics, phone half: exactly-once is per command
    /// ID, so a Run/Steer whose user message never landed and whose command
    /// can never execute again — Rejected (the host's dead-command sweep
    /// terminalized it, synced back over chat2), Expired status, or Pending
    /// past its own TTL (the host will never drain it; an explicit user
    /// retry is exactly the consent to re-send) — gets a FRESH attempt: new
    /// id, same payload and messageId (the host's user-entry pre-write
    /// dedupes by message id). One re-issue per message (latest attempt);
    /// a live pending unexpired attempt for the same message skips it.
    func reissueDeadSends() {
        guard let root = doc.getDeepValue().mapValue,
              let commands = root["commands"]?.listValue, !commands.isEmpty else { return }
        let landed = Set(entries.map(\.id))
        let now = nowMs()
        struct DeadAttempt {
            var kind: String
            var payload: [String: Any]
            var issuedAt: Int64
            var oldId: String
        }
        var latestDead: [String: DeadAttempt] = [:]
        var liveMessageIds: Set<String> = []
        for value in commands {
            guard let m = value.mapValue,
                  m["issuedBy"]?.stringValue == config.deviceId,
                  let kind = m["kind"]?.stringValue, kind == "run" || kind == "steer",
                  let id = m["id"]?.stringValue,
                  let payload = m["payload"]?.mapValue,
                  let messageId = payload["messageId"]?.stringValue,
                  !landed.contains(messageId) else { continue }
            let status = m["status"]?.stringValue ?? "pending"
            let expired = (m["expiresAt"]?.i64Value).map { $0 <= now } ?? false
            if status == "pending", !expired {
                liveMessageIds.insert(messageId)
                continue
            }
            guard status == "rejected" || status == "expired"
                || (status == "pending" && expired) else { continue }
            let issuedAt = m["issuedAt"]?.i64Value ?? 0
            if let existing = latestDead[messageId], existing.issuedAt >= issuedAt { continue }
            guard let object = LoroValue.map(value: payload).jsonObject as? [String: Any] else { continue }
            latestDead[messageId] = DeadAttempt(kind: kind, payload: object,
                                                issuedAt: issuedAt, oldId: id)
        }
        for (messageId, attempt) in latestDead where !liveMessageIds.contains(messageId) {
            roomLog.info("chat2 \(self.chatId, privacy: .public): retry re-issues a dead send attempt (old=\(attempt.oldId, privacy: .public))")
            queueCommand(kind: attempt.kind, payload: attempt.payload)
        }
    }
}

private func encodableJSON<T: Encodable>(_ value: T) -> Any {
    guard let data = try? JSONEncoder().encode(value),
          let obj = try? JSONSerialization.jsonObject(with: data) else { return [:] }
    return obj
}
