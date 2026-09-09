// App session root: sign-in state machine, workspace connection, and the
// per-chat session store cache. Also hosts demo mode — an offline in-memory
// dataset so the UI can be exercised without an edge deployment.

import Foundation
import Network
import Observation
import SwiftUI
import os

@MainActor
@Observable
final class AppModel {
    enum Phase {
        case signedOut
        case pickingOrg(AuthTokens, [AuthOrg])
        case ready
    }

    var phase: Phase = .signedOut
    var workspace: WorkspaceStore?
    var demo: DemoDataset?
    /// Graced connectivity truth — one stream every consumer inherits calm
    /// from (home pill, composer notice, Queued/Failed badges).
    let connectivity = ConnectivityCenter()
    private var sessionStores: [String: SessionStore] = [:]
    private var config: AppConfig?
    @ObservationIgnored private var pathMonitor: NWPathMonitor?
    @ObservationIgnored private var lastPathKey: String?

    // Persisted connection settings.
    @ObservationIgnored @AppStorage("edgeURL") var edgeURLString = "https://edge.zeron.sh"
    @ObservationIgnored @AppStorage("authMode") var authModeRaw = AppConfig.Mode.workos.rawValue
    @ObservationIgnored @AppStorage("userId") var storedUserId = ""
    @ObservationIgnored @AppStorage("orgId") var storedOrgId = ""
    @ObservationIgnored @AppStorage("deviceId") var storedDeviceId = ""

    var deviceId: String {
        if storedDeviceId.isEmpty {
            storedDeviceId = "ios-" + UUID().uuidString.lowercased().prefix(8)
        }
        return storedDeviceId
    }

    var deviceName: String {
        UIDevice.current.name
    }

    /// Deep-link target applied by HomeView on first appearance (set by launch
    /// args in demo mode; simulator-driven screenshots use it).
    var launchRoute: Route?
    /// Screenshot rig: "newsession" / "newspace" presents that sheet on arrival.
    var launchSheet: String?
    /// Screenshot rig: auto-send a canned prompt from the new-session canvas.
    var launchAutosend = false
    /// Screenshot rig: the session composer takes keyboard focus after ~1.5s,
    /// to drive the keyboard-up transcript states headless.
    var launchFocusComposer = false

    func restore() {
        if demo != nil { return }
        DocDisk.prune(keep: 80)
        let args = ProcessInfo.processInfo.arguments
        // Debug-rig config overrides (cfprefsd caching defeats external
        // defaults writes; the app applying them itself always sticks).
        func override(_ flag: String, _ apply: (String) -> Void) {
            if let ix = args.firstIndex(of: flag), ix + 1 < args.count {
                apply(args[ix + 1])
            }
        }
        override("-setedge") { edgeURLString = $0 }
        override("-setmode") { authModeRaw = $0 }
        override("-setuser") { storedUserId = $0 }
        override("-setorg") { storedOrgId = $0 }
        // Simulator rig: seed WorkOS tokens straight into the keychain (the
        // ASWebAuthenticationSession flow can't be driven headlessly).
        override("-setaccess") { Keychain.save($0, key: "accessToken") }
        override("-setrefresh") { Keychain.save($0, key: "refreshToken") }
        if args.contains("-bench") {
            Task { await BenchRunner.run() }
            return
        }
        if args.contains("-e2e") {
            Task { await E2ERunner.run(model: self) }
            return
        }
        if args.contains("-e2e-live") {
            // Reuse the signed-in session, then probe the live relay paths.
            Task {
                try? await Task.sleep(nanoseconds: 500_000_000)
                await E2ERunner.runLive(model: self)
            }
            // fall through to the normal restore below
        }
        if args.contains("-demo") {
            enterDemoMode()
            if let ix = args.firstIndex(of: "-route"), ix + 1 < args.count {
                let spec = args[ix + 1]
                if spec.hasPrefix("chat:") {
                    let chatId = String(spec.dropFirst("chat:".count))
                    launchRoute = .chat(chatId)
                    if args.contains("-big"), let demo {
                        // Scroll-settle stress. Injected BEFORE the transcript
                        // appears, which is the warm-session case: rows are
                        // already there at first layout, so neither the
                        // rows-arrived nor the streamed-growth anchor ever
                        // fires and `.task` is the only thing holding the
                        // bottom — against hundreds of lazily-estimated rows.
                        demo.sessionStore(for: chatId)
                            .setEntries(BenchRunner.syntheticEntries(turns: 120))
                    }
                    if args.contains("-huge"), let demo {
                        // Warm-reopen stress at real-conversation scale — the
                        // estimated-height error grows with row count, and the
                        // settle must converge against it.
                        demo.sessionStore(for: chatId)
                            .setEntries(BenchRunner.syntheticEntries(turns: 600))
                    }
                    if args.contains("-stream"), let demo {
                        // Screenshot rig: kick off the scripted streaming reply.
                        let store = demo.sessionStore(for: chatId)
                        Task { @MainActor in
                            try? await Task.sleep(nanoseconds: 2_000_000_000)
                            store.demoResponder?("Show me the streamed reply path.")
                        }
                    }
                } else if spec.hasPrefix("space:") {
                    launchRoute = .space(String(spec.dropFirst("space:".count)))
                }
            }
            if let ix = args.firstIndex(of: "-sheet"), ix + 1 < args.count {
                launchSheet = args[ix + 1]
            }
            launchAutosend = args.contains("-autosend")
            launchFocusComposer = args.contains("-focuscomposer")
            // Rig: open with an EMPTY transcript that lands in bulk 2.5s
            // later — the live checkpoint-backfill shape (loader → reveal).
            if args.contains("-hydrate-late"), case .chat(let lateId)? = launchRoute, let demo {
                let store = demo.sessionStore(for: lateId)
                let full = store.entries
                store.setEntries([])
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 2_500_000_000)
                    store.setEntries(full)
                }
            }
            // Animation rig: "-archive-after chatId:secs" / "-unarchive-after
            // chatId:secs" fire the same animated mutation the swipe actions
            // use, so the list hand-off can be recorded headless.
            func scheduledToggle(_ flag: String, archived: Bool) {
                guard let ix = args.firstIndex(of: flag), ix + 1 < args.count else { return }
                let parts = args[ix + 1].split(separator: ":")
                guard parts.count == 2, let secs = Double(parts[1]) else { return }
                let chatId = String(parts[0])
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: UInt64(secs * 1_000_000_000))
                    withAnimation(Motion.resort) {
                        if archived { self.archive(chatId: chatId) }
                        else { self.unarchive(chatId: chatId) }
                    }
                }
            }
            scheduledToggle("-archive-after", archived: true)
            scheduledToggle("-unarchive-after", archived: false)
            return
        }
        startPathMonitor()
        guard let url = URL(string: edgeURLString), !storedUserId.isEmpty, !storedOrgId.isEmpty else {
            return
        }
        let mode = AppConfig.Mode(rawValue: authModeRaw) ?? .workos
        switch mode {
        case .dev:
            connect(url: url, mode: .dev, userId: storedUserId, orgId: storedOrgId,
                    tokens: nil, devBearer: devBearer(userId: storedUserId, orgId: storedOrgId))
        case .workos:
            guard let access = Keychain.load(key: "accessToken"),
                  let refresh = Keychain.load(key: "refreshToken") else { return }
            connect(url: url, mode: .workos, userId: storedUserId, orgId: storedOrgId,
                    tokens: AuthTokens(accessToken: access, refreshToken: refresh), devBearer: nil)
        }
    }

    // MARK: Sign-in flows

    /// WorkOS paste-code exchange. Returns the org list for the picker (or
    /// connects straight away when exactly one org exists).
    func signIn(edgeURL: URL, code: String) async throws {
        let client = AuthClient(baseURL: edgeURL)
        let (user, tokens) = try await client.exchange(code: code)
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.workos.rawValue
        storedUserId = user.id
        let orgs = try await client.orgs(accessToken: tokens.accessToken)
        if let only = orgs.first, orgs.count == 1 {
            try await selectOrg(only, tokens: tokens)
        } else if orgs.isEmpty {
            throw AuthError.http(403, "No organizations for this account")
        } else {
            phase = .pickingOrg(tokens, orgs)
        }
    }

    func selectOrg(_ org: AuthOrg, tokens: AuthTokens) async throws {
        guard let url = URL(string: edgeURLString) else { return }
        // Re-scope the access token to the org (adds the org_id claim).
        let client = AuthClient(baseURL: url)
        let scoped = try await client.refresh(refreshToken: tokens.refreshToken,
                                              organizationId: org.organizationId)
        Keychain.save(scoped.accessToken, key: "accessToken")
        Keychain.save(scoped.refreshToken, key: "refreshToken")
        storedOrgId = org.organizationId
        connect(url: url, mode: .workos, userId: storedUserId, orgId: org.organizationId,
                tokens: scoped, devBearer: nil)
    }

    /// Dev-mode edge (AUTH_MODE=dev): bearer = "userId@orgId".
    func signInDev(edgeURL: URL, userId: String, orgId: String) {
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.dev.rawValue
        storedUserId = userId
        storedOrgId = orgId
        connect(url: edgeURL, mode: .dev, userId: userId, orgId: orgId,
                tokens: nil, devBearer: devBearer(userId: userId, orgId: orgId))
    }

    func enterDemoMode() {
        demo = DemoDataset.standard()
        phase = .ready
    }

    func signOut() {
        workspace?.stop()
        workspace = nil
        sessionStores.values.forEach { $0.stop() }
        sessionStores.removeAll()
        config = nil
        demo = nil
        Keychain.delete(key: "accessToken")
        Keychain.delete(key: "refreshToken")
        DocDisk.wipeAll()  // local doc state belongs to the signed-in identity
        storedUserId = ""
        storedOrgId = ""
        phase = .signedOut
    }

    private func devBearer(userId: String, orgId: String) -> String {
        orgId.isEmpty ? userId : "\(userId)@\(orgId)"
    }

    private func connect(url: URL, mode: AppConfig.Mode, userId: String, orgId: String,
                         tokens: AuthTokens?, devBearer: String?) {
        let config = AppConfig(edgeURL: url, mode: mode, userId: userId, orgId: orgId,
                               deviceId: deviceId, deviceName: deviceName,
                               tokens: tokens, devBearer: devBearer)
        self.config = config
        let store = WorkspaceStore(config: config)
        workspace = store
        store.start()
        startConnectivity()
        phase = .ready
    }

    /// Wire the graced-connectivity recompute over the live stores (the
    /// engine's 1s compute_connectivity, phone edition).
    private func startConnectivity() {
        connectivity.registryConnected = { [weak self] in
            guard let self, self.demo == nil, let workspace = self.workspace else { return true }
            return workspace.connected
        }
        connectivity.chatRooms = { [weak self] in
            guard let self else { return [] }
            return self.sessionStores.compactMap { id, store in
                store.roomActive ? (id: id, connected: store.connected) : nil
            }
        }
        connectivity.hasPendingSends = { [weak self] in
            self?.sessionStores.values.contains { !$0.pendingSends.isEmpty } ?? false
        }
        connectivity.start()
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }

    // "Connected" for the header spinner means "server state has reached this
    // session" — over the socket OR the HTTPS pull (which lands in ~1 RTT and
    // is the only transport airplane wifi permits).
    var connected: Bool { demo != nil || workspace?.connected == true || workspace?.synced == true }

    var overviewChats: [Chat] {
        if let demo {
            let liveIds = Set(demo.spaces.map(\.id))
            let live = demo.chats.filter { !$0.archived && $0.spaceId.map(liveIds.contains) == true }
            return sortActive(live)
        }
        return workspace?.overviewChats ?? []
    }

    func chats(in spaceId: String) -> [Chat] {
        if let demo {
            return sortActive(demo.chats.filter { !$0.archived && $0.spaceId == spaceId })
        }
        return workspace?.chats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        (demo?.chats ?? workspace?.chats)?.first { $0.id == id }
    }

    /// state.rs `space_for_chat` — nil for a dangling/missing space_id.
    func space(for chat: Chat) -> Space? {
        guard let spaceId = chat.spaceId else { return nil }
        return spaces.first { $0.id == spaceId }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        if let demo {
            return chatIndicator(chat: chat, live: effectiveStatus(demo.sessions[chat.id], now: nowMs()))
        }
        return workspace?.indicator(for: chat) ?? .idle
    }

    func changeRequest(for chat: Chat) -> ChangeRequestSummary? {
        if let demo { return demo.changeRequests[chat.id] }
        return workspace?.changeRequest(for: chat)
    }

    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        chats(in: spaceId).map { indicator(for: $0) }.min { $0.rawValue < $1.rawValue }
    }

    func deviceName(_ deviceId: String) -> String {
        (demo?.devices ?? workspace?.devices)?.first { $0.id == deviceId }?.name ?? deviceId
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        if let demo {
            guard let seen = demo.devices.first(where: { $0.id == deviceId })?.lastSeenAt else { return false }
            return nowMs() - seen < presenceFreshMs
        }
        return workspace?.deviceOnline(deviceId) ?? false
    }

    /// Live harness catalog from the space's owning device (Settings → Agents
    /// gates which agents a device offers); static pair when unreachable.
    func listHarnesses(space: Space) async -> [HarnessInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.harnesses
        }
        if let live = await workspace?.listHarnesses(deviceId: space.deviceId),
           !live.isEmpty {
            return live
        }
        return HarnessCatalog.harnesses
    }

    /// Live model catalog from the space's owning device (the desktop's
    /// "catalog source = the device that runs the session" rule); static
    /// fallback when the device is unreachable.
    func listModels(space: Space, harness: String) async -> [ModelInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.models(for: harness)
        }
        if let live = await workspace?.listModels(deviceId: space.deviceId, harness: harness),
           !live.isEmpty {
            return live
        }
        return HarnessCatalog.models(for: harness)
    }

    /// Refs of the space's repo (git spaces only).
    func listRefs(space: Space) async -> [RepoRef]? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)
            return demo.listRefs(spacePath: space.path)
        }
        return await workspace?.listRefs(deviceId: space.deviceId, repoPath: space.path)
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder.
    /// Returns an error message, or nil on success.
    func switchSpaceRef(space: Space, refName: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: space.path, refName: refName)
            return nil
        }
        guard let workspace else { return "Not connected" }
        return await workspace.switchRef(deviceId: space.deviceId,
                                         repoPath: space.path, refName: refName)
    }

    /// Mid-session ref switch (desktop switch_session_ref): retarget onto the
    /// ref's existing worktree (row writes, no git), else checkout in the
    /// session's own cwd on the host. Returns an error message or nil.
    func switchSessionRef(chat: Chat, ref: RepoRef) async -> String? {
        guard let cwd = chat.cwd else { return "Session has no working folder" }
        if let worktree = ref.worktreePath {
            if worktree == cwd { return nil }  // already here
            if let demo {
                if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                    demo.chats[ix].cwd = worktree
                    demo.chats[ix].branch = ref.name
                }
                return nil
            }
            workspace?.setChatCheckout(chatId: chat.id, cwd: worktree, branch: ref.name)
            return nil
        }
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: cwd, refName: ref.name)
            if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                demo.chats[ix].branch = ref.name
            }
            return nil
        }
        guard let workspace else { return "Not connected" }
        let error = await workspace.switchRef(deviceId: chat.deviceId,
                                              repoPath: cwd, refName: ref.name)
        if error == nil {
            // The host's HEAD watcher reconciles chat.branch eventually;
            // stamp it optimistically so the UI answers immediately.
            workspace.setChatCheckout(chatId: chat.id, cwd: cwd, branch: ref.name)
        }
        return error
    }

    /// CreateWorktree off the base ref; returns the new worktree's path.
    func createWorktree(space: Space, base: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 250_000_000)
            return demo.createWorktree(spacePath: space.path, base: base)
        }
        return await workspace?.createWorktree(deviceId: space.deviceId,
                                               spaceId: space.id,
                                               repoPath: space.path, branch: base)
    }

    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) -> String? {
        if let demo {
            let id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            demo.chats.append(Chat(id: id, deviceId: space.deviceId, title: nil, archived: false,
                                   cwd: cwd ?? space.path, branch: branch, checkoutId: nil,
                                   config: chatConfig, lastMessagePreview: nil, lastMessageAt: nil,
                                   createdAt: nowMs(), spaceId: space.id, lastSeenAt: nowMs()))
            return id
        }
        return workspace?.createChat(space: space, config: chatConfig, branch: branch, cwd: cwd)
    }

    /// Browse folders on a remote device (the desktop add-space palette's data
    /// path). Demo mode serves a canned tree; live mode asks the device over
    /// the relay.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)  // feel like a network hop
            let target = path ?? demo.homePath(deviceId: deviceId)
            return demo.listFolders(deviceId: deviceId, path: target)
        }
        return await workspace?.listFolders(deviceId: deviceId, path: path)
    }

    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String? {
        if let demo {
            if let existing = demo.spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
                return existing.id
            }
            let id = "space-\(UUID().uuidString.lowercased().prefix(8))"
            demo.spaces.append(Space(id: id, deviceId: deviceId, path: path, name: nil,
                                     gitDetected: gitDetected, gitCheckedAt: nil, checkoutId: nil,
                                     createdAt: nowMs()))
            return id
        }
        return await workspace?.createSpace(deviceId: deviceId, path: path, gitDetected: gitDetected)
    }

    /// Archived chats under the same scope as the list above the shelf.
    func archivedChats(in spaceId: String? = nil) -> [Chat] {
        if let demo {
            return sortActive(demo.chats.filter {
                $0.archived && (spaceId == nil || $0.spaceId == spaceId)
            })
        }
        return workspace?.archivedChats(in: spaceId) ?? []
    }

    func archive(chatId: String) { setArchived(chatId: chatId, archived: true) }
    func unarchive(chatId: String) { setArchived(chatId: chatId, archived: false) }

    private func setArchived(chatId: String, archived: Bool) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = archived
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: archived)
    }

    func setChatConfig(chatId: String, config: ChatConfig) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].config = config
            }
            return
        }
        workspace?.setChatConfig(chatId: chatId, config: config)
    }

    func markSeen(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].lastSeenAt = nowMs()
            }
            return
        }
        workspace?.markSeen(chatId: chatId)
    }

    /// Persist every open doc now (app backgrounding).
    func flushDocs() {
        workspace?.flushToDisk()
        sessionStores.values.forEach { $0.flushToDisk() }
    }

    /// Foreground hook: kick every room NOW (see ChatRoomClient.kick) — after
    /// a suspension the workspace room in particular stayed dead while chat
    /// views reconnected on open, freezing sidebar rows and Working
    /// indicators against perfectly live transcripts (2026-08-04). Also the
    /// focus fast path (PR #168): probe {edge}/health (3s) and broadcast the
    /// online event on success, so every PARKED backoff (not just the rooms
    /// the kick reaches) lands a redial in ~1 RTT.
    func foregrounded() {
        kickAllRooms()
        probeEdgeHealth()
    }

    private func probeEdgeHealth() {
        guard let config, demo == nil else { return }
        Task.detached {
            var request = URLRequest(url: config.edgeURL.appending(path: "health"))
            request.timeoutInterval = 3
            guard let (_, response) = try? await URLSession.shared.data(for: request),
                  (response as? HTTPURLResponse)?.statusCode == 200 else { return }
            OnlineBus.shared.notifyOnline()
        }
    }

    private func kickAllRooms() {
        workspace?.kickRoom()
        // Deliver any roomGen flips that landed while the store had no open
        // view, then kick every room — registry first and instantly, chat
        // rooms trickled one per 200ms in attention order. Post-suspend and
        // path-recovery kicks redial dead sockets; a simultaneous N-socket
        // redial competed with the registry (the sidebar the user is
        // actually looking at) on thin links.
        if let workspace {
            for chat in workspace.chats {
                sessionStores[chat.id]?.updateRoomGen(chat.roomGen)
            }
        }
        var delay: UInt64 = 0
        var kicked = Set<String>()
        for chat in overviewChats {
            // Dial-held stores stay held: a kick force-dials, and sweeping 46
            // of them on every foreground/path flap is the stampede the warm
            // cap exists to prevent. Held chats reconnect on open.
            guard let store = sessionStores[chat.id], !store.isDialHeld else { continue }
            kicked.insert(chat.id)
            scheduleKick(store, afterNs: delay)
            delay += 200_000_000
        }
        for (id, store) in sessionStores where !kicked.contains(id) && !store.isDialHeld {
            scheduleKick(store, afterNs: delay)
            delay += 200_000_000
        }
    }

    private func scheduleKick(_ store: SessionStore, afterNs delay: UInt64) {
        Task { @MainActor in
            if delay > 0 { try? await Task.sleep(nanoseconds: delay) }
            store.kickRoom()
        }
    }

    /// Kick rooms the moment the network path recovers or hops interfaces
    /// (wifi drop-and-return while foregrounded, wifi→cellular handover).
    /// Without this the clients sleep out their full reconnect backoff — up
    /// to 30s of dead sidebar on exactly the flaky networks (airplane wifi)
    /// where the OS knows recovery happened the instant it did. Kicks are
    /// idempotent: fresh backoff + immediate redial or a deadline-checked
    /// probe on a session that looks alive.
    private func startPathMonitor() {
        guard pathMonitor == nil else { return }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            // net_path.rs semantics: only a definitive "unsatisfied" parks —
            // requiresConnection/other stay optimistic (a confused monitor
            // can only make us dial too much, never go silent). Every
            // satisfied report also broadcasts online: satisfied→satisfied
            // updates are interface handovers (wifi→cellular), and the old
            // sockets are dead on the new path; redundant kicks are free
            // because waiters drain stale events.
            OnlineBus.shared.setPathOnline(path.status != .unsatisfied)
            if path.status == .satisfied {
                OnlineBus.shared.notifyOnline()
            }
            // Interface set is part of the key: a satisfied→satisfied hop
            // (wifi→cellular) silently kills established sockets too.
            let key = path.status == .satisfied
                ? "up:" + path.availableInterfaces.map(\.name).sorted().joined(separator: ",")
                : "down"
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.connectivity.setPathOffline(path.status == .unsatisfied)
                let previous = self.lastPathKey
                self.lastPathKey = key
                // First callback reports the initial state — nothing to revive.
                guard let previous, previous != key, path.status == .satisfied else { return }
                roomLog.info("network path recovered (\(key, privacy: .public)); kicking rooms")
                self.kickAllRooms()
            }
        }
        monitor.start(queue: DispatchQueue(label: "zeron.path-monitor"))
        pathMonitor = monitor
    }

    /// Diagnostics access (live e2e probe).
    var diagnosticsConfig: AppConfig? { config }

    // MARK: Session stores

    func sessionStore(for chat: Chat) -> SessionStore? {
        if let demo { return demo.sessionStore(for: chat.id) }
        guard let config else { return nil }
        if let existing = sessionStores[chat.id] {
            existing.hostDeviceId = chat.deviceId
            // The registry flip to chat2 can land while the store is open —
            // views re-derive `chat` from the registry on every change, so
            // this accessor is the flip's delivery path.
            existing.updateRoomGen(chat.roomGen)
            // An open view wants live sync NOW — any preload dial-hold ends.
            existing.releaseDial()
            return existing
        }
        let store = SessionStore(chatId: chat.id, config: config)
        store.hostDeviceId = chat.deviceId
        store.hostLiveness = { [weak self] deviceId in
            self?.workspace?.peerLiveness(deviceId) ?? .unknown
        }
        sessionStores[chat.id] = store
        store.start()
        store.updateRoomGen(chat.roomGen)
        return store
    }

    // MARK: Delivery truth (state.rs chat_delivery_degraded / send_* ports)

    /// Queued-attachment version gate (composer.rs QUEUED_ATTACHMENTS_MIN):
    /// the host must defer commands with pending:// refs, or the send would
    /// dispatch with unresolvable paths.
    static let queuedAttachmentsMin = (0, 2, 12)

    func hostSupportsQueuedAttachments(_ chat: Chat) -> Bool {
        hostSupportsQueuedAttachmentsOn(deviceId: chat.deviceId)
    }

    func hostSupportsQueuedAttachmentsOn(deviceId: String) -> Bool {
        guard demo == nil else { return false }
        return workspace?.deviceVersionAtLeast(deviceId, Self.queuedAttachmentsMin) ?? false
    }

    /// Whether a send to this chat would queue rather than deliver promptly:
    /// OS offline, the chat's room degraded (graced), or the host device
    /// presence-dark. Every chat is remote-hosted on the phone — there is no
    /// "locally hosted, never degraded" branch.
    func chatDeliveryDegraded(_ chat: Chat) -> Bool {
        guard demo == nil else { return false }
        if connectivity.state == .offline { return true }
        if let store = sessionStores[chat.id], store.roomActive {
            if connectivity.degradedChats.contains(chat.id) { return true }
        } else if connectivity.state != .connected {
            return true
        }
        if !deviceOnline(chat.deviceId) { return true }
        return false
    }

    /// The user-visible truth of a chat's oldest unadopted send. `failed`
    /// (unadopted past the 2-minute grace, with a retry affordance) wins over
    /// `queued` (pending on a degraded path); a healthy in-flight send reads
    /// `sending`. nil = nothing pending.
    func sendState(for chat: Chat, now: Int64 = nowMs()) -> SendState? {
        guard demo == nil, let store = sessionStores[chat.id],
              let oldest = store.pendingSends.map(\.started).min() else { return nil }
        if now - oldest > undeliveredGraceMs { return .failed }
        if chatDeliveryDegraded(chat) { return .queued }
        return .sending
    }

    func releaseSessionStore(chatId: String) {
        // Preloaded stores stay warm — nothing to evict on navigation.
    }

    /// Warm every non-archived session: stores hydrate from disk instantly
    /// so opening a session never shows a loading state. The room DIALS are
    /// held and released one per 300ms in attention order — N simultaneous
    /// TLS handshakes at launch competed with the registry dial for a thin
    /// uplink (and, pre-single-flight, raced N token refreshes), which was
    /// the cold-open "connecting…" stall. Opening a session releases its
    /// hold immediately (sessionStore(for:) above).
    /// Sessions that keep a live socket without an open view. Everything else
    /// hydrates from disk but dials on demand: 46 background joins (TLS +
    /// hello + state each) drowned a 450kbps link for tens of seconds at
    /// every cold open and network kick, for transcripts nobody was reading —
    /// sidebar status (Working, presence, titles) rides the registry room, so
    /// an undialed chat's row stays live regardless, and opening it releases
    /// its dial instantly.
    static let warmDialCap = 8

    func preloadSessions() {
        guard demo == nil, let config else { return }
        var stagger: UInt64 = 0
        var released = 0
        for chat in overviewChats where sessionStores[chat.id] == nil {
            let store = SessionStore(chatId: chat.id, config: config)
            store.hostDeviceId = chat.deviceId
            store.hostLiveness = { [weak self] deviceId in
                self?.workspace?.peerLiveness(deviceId) ?? .unknown
            }
            sessionStores[chat.id] = store
            store.start(holdDial: true)
            store.updateRoomGen(chat.roomGen)
            guard released < Self.warmDialCap else { continue }
            released += 1
            let delay = stagger
            Task { @MainActor in
                // The registry (the sidebar the user is looking at) gets the
                // pipe to itself first: on a 240kbps link, warm chat dials
                // racing the registry's own handshake+state pushed the
                // connect spinner from ~1.5s to ~7s (NLC Edge, 2026-08-17).
                // An open view still dials instantly via releaseDial.
                let start = DispatchTime.now()
                while !(self.workspace?.connected ?? false),
                      DispatchTime.now().uptimeNanoseconds &- start.uptimeNanoseconds < 10_000_000_000 {
                    try? await Task.sleep(nanoseconds: 200_000_000)
                }
                if delay > 0 { try? await Task.sleep(nanoseconds: delay) }
                store.releaseDial()
            }
            stagger += 300_000_000
        }
    }
}
