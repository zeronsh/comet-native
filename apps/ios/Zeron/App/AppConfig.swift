// Session-wide connection config: edge base URL, identity, token minting for
// room sockets (WS auth rides the URL query — sockets can't set headers), and
// the durable-nudge POST. Thread-safe (rooms call in from their actors).

import Foundation

final class AppConfig: @unchecked Sendable {
    enum Mode: String {
        case workos
        case dev
    }

    let edgeURL: URL
    let mode: Mode
    let userId: String
    let orgId: String
    let deviceId: String
    let deviceName: String

    let vault: MobileVault
    let vaultPersistence: VaultPersistence
    enum SyncAccess: Sendable, Equatable { case blocked, legacy, encrypted }
    private var access: SyncAccess = .blocked
    private var invalidated = false
    private let lock = NSLock()

    var syncAccess: SyncAccess { lock.withLock { access } }
    func setSyncAccess(_ value: SyncAccess) { lock.withLock { access = invalidated ? .blocked : value } }
    func permitsSync(encrypted: Bool) -> Bool {
        lock.withLock { !invalidated && access == (encrypted ? .encrypted : .legacy) }
    }
    var vaultClient: MobileVaultClient {
        MobileVaultClient(origin: edgeURL, orgId: orgId, token: { [weak self] in await self?.currentToken() })
    }
    func invalidate() {
        lock.withLock {
            invalidated = true
            access = .blocked
            tokens = nil
            devBearer = nil
            refreshTask?.cancel()
            refreshTask = nil
        }
    }
    private var tokens: AuthTokens?
    private var devBearer: String?
    /// In-flight refresh shared by every caller (single-flight). WorkOS
    /// refresh tokens are SINGLE-USE (rotated per use, desktop auth.rs
    /// refresh_gate): without this, a cold launch's N room dials raced N
    /// concurrent refreshes with the same token — one won and rotated it,
    /// the rest failed, dialed with the dead access token, got rejected,
    /// and every socket sat in backoff. That was the 5–10s "connecting"
    /// stall on every app open past token expiry (~5 min).
    private var refreshTask: Task<String?, Never>?

    init(edgeURL: URL, mode: Mode, userId: String, orgId: String,
         deviceId: String, deviceName: String,
         tokens: AuthTokens? = nil, devBearer: String? = nil) {
        self.edgeURL = edgeURL
        self.mode = mode
        self.userId = userId
        self.orgId = orgId
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.tokens = tokens
        self.devBearer = devBearer
        vaultPersistence = VaultPersistence(origin: edgeURL, orgId: orgId, userId: userId)
        vault = MobileVault(persistence: vaultPersistence, orgId: orgId, userId: userId)
    }

    func updateTokens(_ new: AuthTokens) {
        lock.withLock {
            if !invalidated { tokens = new }
        }
    }

    /// Current bearer, refreshing the WorkOS access token when needed.
    func currentToken() async -> String? {
        switch mode {
        case .dev:
            return lock.withLock { devBearer }
        case .workos:
            let current = lock.withLock { tokens }
            guard let current else { return nil }
            if !Self.isExpired(jwt: current.accessToken) {
                return current.accessToken
            }
            return await refreshedToken(current: current)
        }
    }

    /// Join (or start) the one in-flight refresh. The task clears itself
    /// under the lock as its last act, so a caller either joins a live
    /// refresh or starts a fresh one — never a second concurrent POST.
    private func refreshedToken(current: AuthTokens) async -> String? {
        let task = lock.withLock {
            if let existing = refreshTask {
                return existing
            }

            let task = Task<String?, Never> { [edgeURL, orgId] in
                let client = AuthClient(baseURL: edgeURL)
                let refreshed = try? await client.refresh(refreshToken: current.refreshToken,
                                                          organizationId: orgId)
                if let refreshed {
                    let accepted = self.lock.withLock {
                        guard !self.invalidated else { return false }
                        self.tokens = refreshed
                        Keychain.save(refreshed.accessToken, key: "accessToken")
                        Keychain.save(refreshed.refreshToken, key: "refreshToken")
                        return true
                    }
                    guard accepted else { return nil }
                } else {
                    roomLog.error("auth: token refresh failed; using expired access token (server will reject and rooms will redial)")
                }
                self.lock.withLock {
                    self.refreshTask = nil
                }
                // Failure falls back to the expired token: let the server
                // reject; the rooms' backoff redials retry through here.
                return self.lock.withLock { self.invalidated ? nil : (refreshed?.accessToken ?? current.accessToken) }
            }
            refreshTask = task
            return task
        }
        return await task.value
    }

    private func syncToken(encrypted: Bool) async -> String? {
        guard permitsSync(encrypted: encrypted), let token = await currentToken(), permitsSync(encrypted: encrypted) else { return nil }
        return token
    }

    private var wsBase: URL {
        var components = URLComponents(url: edgeURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        return components.url!
    }

    /// The workspace registry room (docs/registry-sync.md) — the row-table
    /// replacement for the old ws Loro workspace doc.
    func registrySocketURL(encrypted: Bool = false) async -> URL? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = wsBase.appending(path: "registry/\(orgId)\(encrypted ? "/e1" : "")/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token),
                                URLQueryItem(name: "device", value: deviceId)])
        return url
    }

    /// The chat2 log-relay room (docs/chat2-sync.md B) — replaces the s2
    /// session rooms, which mobile no longer dials at all. `device` rides the
    /// URL so the DO can attribute sockets and honor excludeOwn backfills.
    func chat2SocketURL(chatId: String, encrypted: Bool = false) async -> URL? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = wsBase.appending(path: "chat2/\(encrypted ? MobileVault.encryptedRoomId(chatId) : chatId)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token),
                                URLQueryItem(name: "device", value: deviceId)])
        return url
    }

    /// GET /chat2/{chatId}/checkpoint — the Range-resumable doc snapshot
    /// (auth via bearer header; the caller adds Range on resume).
    func chat2CheckpointRequest(chatId: String, encrypted: Bool = false) async -> URLRequest? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var request = URLRequest(url: edgeURL.appending(path: "chat2/\(encrypted ? MobileVault.encryptedRoomId(chatId) : chatId)/checkpoint"))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// GET /chat2/{chatId}/rows?after= — pull over plain HTTPS: one request
    /// collapses the socket's connect→hello→state→rowsReq→backfill, and it
    /// works on networks that strip WS upgrades (airplane wifi).
    func chat2RowsRequest(chatId: String, after: UInt64, encrypted: Bool = false) async -> URLRequest? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = edgeURL.appending(path: "chat2/\(encrypted ? MobileVault.encryptedRoomId(chatId) : chatId)/rows")
        url.append(queryItems: [URLQueryItem(name: "after", value: String(after)),
                                URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// POST /chat2/{chatId}/rows?batchId= — push over plain HTTPS (batchId
    /// dedupe makes replays no-ops); body is the raw update batch.
    func chat2PushRequest(chatId: String, batchId: String, encrypted: Bool = false) async -> URLRequest? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = edgeURL.appending(path: "chat2/\(encrypted ? MobileVault.encryptedRoomId(chatId) : chatId)/rows")
        url.append(queryItems: [URLQueryItem(name: "batchId", value: batchId),
                                URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// GET /registry/{orgId}/rows?since= — the WS hello's delta answer over
    /// plain HTTPS. `beat=1` doubles as a presence beat.
    func registryRowsRequest(since: UInt64?, encrypted: Bool = false) async -> URLRequest? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = edgeURL.appending(path: "registry/\(orgId)\(encrypted ? "/e1" : "")/rows")
        var items = [URLQueryItem(name: "device", value: deviceId),
                     URLQueryItem(name: "beat", value: "1")]
        if let since { items.append(URLQueryItem(name: "since", value: String(since))) }
        url.append(queryItems: items)
        // Bearer header, never ?token=: HTTP supports headers (unlike WS
        // upgrades), and query strings can reach request logs.
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// POST /registry/{orgId}/push — one op batch over plain HTTPS (LWW
    /// clocks make replays apply zero ops).
    func registryPushRequest(encrypted: Bool = false) async -> URLRequest? {
        guard let token = await syncToken(encrypted: encrypted) else { return nil }
        var url = edgeURL.appending(path: "registry/\(orgId)\(encrypted ? "/e1" : "")/push")
        url.append(queryItems: [URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        return request
    }

    /// Decode the JWT payload's `exp` (60s early-refresh margin). Unparseable
    /// tokens read as non-expired — the server is the arbiter.
    private static func isExpired(jwt: String) -> Bool {
        let segments = jwt.split(separator: ".")
        guard segments.count == 3 else { return false }
        var base64 = String(segments[1]).replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while base64.count % 4 != 0 { base64 += "=" }
        guard let data = Data(base64Encoded: base64),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let exp = obj["exp"] as? TimeInterval else { return false }
        return Date().timeIntervalSince1970 > exp - 60
    }

    /// GET /device/{deviceId}/status → whether the device's relay HOST socket
    /// is currently attached (distinct from workspace presence).
    func deviceStatus(deviceId: String) async -> String {
        guard let token = await currentToken() else { return "no-token" }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/status"))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              let http = response as? HTTPURLResponse else { return "unreachable" }
        return "http=\(http.statusCode) body=\(String(data: data, encoding: .utf8) ?? "")"
    }

    /// POST /device/{deviceId}/nudge {chatId} — wake a cold host to drain the
    /// command queue.
    func nudge(deviceId: String, chatId: String) async {
        guard let token = await currentToken() else { return }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/nudge"))
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["chatId": chatId])
        _ = try? await URLSession.shared.data(for: request)
    }
}
