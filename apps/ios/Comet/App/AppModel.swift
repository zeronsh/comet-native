// App session root: sign-in state machine, workspace connection, and the
// per-chat session store cache. Also hosts demo mode — an offline in-memory
// dataset so the UI can be exercised without an edge deployment.

import Foundation
import Observation
import SwiftUI

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
    private var sessionStores: [String: SessionStore] = [:]
    private var config: AppConfig?

    // Persisted connection settings.
    @ObservationIgnored @AppStorage("edgeURL") var edgeURLString = "https://edge.comet.zeron.sh"
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

    func restore() {
        if demo != nil { return }
        let args = ProcessInfo.processInfo.arguments
        if args.contains("-demo") {
            enterDemoMode()
            if let ix = args.firstIndex(of: "-route"), ix + 1 < args.count {
                let spec = args[ix + 1]
                if spec.hasPrefix("chat:") {
                    let chatId = String(spec.dropFirst("chat:".count))
                    launchRoute = .chat(chatId)
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
            return
        }
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
        phase = .ready
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }

    var connected: Bool { demo != nil || workspace?.connected == true }

    var overviewChats: [Chat] {
        if let demo {
            let liveIds = Set(demo.spaces.map(\.id))
            let live = demo.chats.filter { !$0.archived && $0.spaceId.map(liveIds.contains) == true }
            return attentionSort(live) { indicator(for: $0) }
        }
        return workspace?.overviewChats ?? []
    }

    func chats(in spaceId: String) -> [Chat] {
        if let demo {
            return demo.chats.filter { !$0.archived && $0.spaceId == spaceId }
                .sorted { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }
        }
        return workspace?.chats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        (demo?.chats ?? workspace?.chats)?.first { $0.id == id }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        if let demo {
            return chatIndicator(chat: chat, live: effectiveStatus(demo.sessions[chat.id], now: nowMs()))
        }
        return workspace?.indicator(for: chat) ?? .idle
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

    func archive(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = true
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: true)
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

    // MARK: Session stores

    func sessionStore(for chat: Chat) -> SessionStore? {
        if let demo { return demo.sessionStore(for: chat.id) }
        guard let config else { return nil }
        if let existing = sessionStores[chat.id] {
            existing.hostDeviceId = chat.deviceId
            return existing
        }
        let store = SessionStore(chatId: chat.id, config: config)
        store.hostDeviceId = chat.deviceId
        sessionStores[chat.id] = store
        store.start()
        return store
    }

    func releaseSessionStore(chatId: String) {
        // Keep recent stores warm; cap the cache to avoid socket sprawl.
        if sessionStores.count > 6, let victim = sessionStores.keys.first(where: { $0 != chatId }) {
            sessionStores[victim]?.stop()
            sessionStores.removeValue(forKey: victim)
        }
    }
}
