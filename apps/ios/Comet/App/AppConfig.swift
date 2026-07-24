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

    private let lock = NSLock()
    private var tokens: AuthTokens?
    private var devBearer: String?

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
    }

    func updateTokens(_ new: AuthTokens) {
        lock.lock(); defer { lock.unlock() }
        tokens = new
    }

    /// Current bearer, refreshing the WorkOS access token when needed.
    func currentToken() async -> String? {
        switch mode {
        case .dev:
            lock.lock(); defer { lock.unlock() }
            return devBearer
        case .workos:
            lock.lock()
            let current = tokens
            lock.unlock()
            guard let current else { return nil }
            if !Self.isExpired(jwt: current.accessToken) {
                return current.accessToken
            }
            let client = AuthClient(baseURL: edgeURL)
            guard let refreshed = try? await client.refresh(refreshToken: current.refreshToken,
                                                            organizationId: orgId) else {
                return current.accessToken  // let the server reject; backoff redials
            }
            updateTokens(refreshed)
            Keychain.save(refreshed.accessToken, key: "accessToken")
            Keychain.save(refreshed.refreshToken, key: "refreshToken")
            return refreshed.accessToken
        }
    }

    private var wsBase: URL {
        var components = URLComponents(url: edgeURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        return components.url!
    }

    func workspaceSocketURL() async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "workspace/\(orgId)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token)])
        return url
    }

    func sessionSocketURL(chatId: String) async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "session/\(chatId)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token)])
        return url
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
