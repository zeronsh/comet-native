// Device-room relay RPC client — dials a device's room on the edge as a
// `client` peer and speaks ControlRpc to the HOST engine over a virtual
// socket (crates/rpc/src/device_room.rs + edge/src/device-room.ts).
//
// Frame codec (binary WS messages): uleb128(headerLen) ‖ headerJSON ‖ payload.
// Header key order MUST be {"s","k","to","from"} (byte parity with both
// implementations); clients never set `to`/`from` — the DO stamps `from`.
// RPC payloads are ndjson ControlRpc frames: {id, method, params} out,
// {id, ok|err|item|done} back. Relay control frames (kind " relay" — leading
// space is part of the constant) signal host_offline/host_closed.

import Foundation

enum RelayError: LocalizedError {
    case notConnected
    case hostOffline
    case rpc(String)
    case timeout

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected to the device"
        case .hostOffline: return "The device is offline"
        case .rpc(let message): return message
        case .timeout: return "The device didn't respond"
        }
    }
}

actor DeviceRelayClient {
    static let rpcKind = "rpc"
    static let relayKind = " relay"  // leading space is intentional

    private let deviceId: String
    private let config: AppConfig
    private let connId = UUID().uuidString.lowercased()

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<Result<Data, RelayError>, Never>] = [:]
    private var connected = false

    init(deviceId: String, config: AppConfig) {
        self.deviceId = deviceId
        self.config = config
    }

    // MARK: Lifecycle

    private func connect() async throws {
        if connected, socket != nil { return }
        guard let token = await config.currentToken() else { throw RelayError.notConnected }
        var components = URLComponents(url: config.edgeURL.appending(path: "device/\(deviceId)/ws"),
                                       resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        components.queryItems = [
            URLQueryItem(name: "role", value: "client"),
            URLQueryItem(name: "connId", value: connId),
            URLQueryItem(name: "token", value: token),
        ]
        let task = URLSession.shared.webSocketTask(with: components.url!)
        socket = task
        task.resume()
        connected = true

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.socket else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message)
                } catch {
                    await self.teardown(error: .hostOffline)
                    return
                }
            }
        }
        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 30_000_000_000)
                guard let self else { return }
                await self.sendPing()
            }
        }
    }

    func close() {
        teardown(error: .notConnected)
    }

    private func teardown(error: RelayError) {
        receiveTask?.cancel()
        pingTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connected = false
        let waiting = pending
        pending.removeAll()
        for (_, continuation) in waiting {
            continuation.resume(returning: .failure(error))
        }
    }

    private func sendPing() async {
        try? await socket?.send(.string("ping"))
    }

    // MARK: RPC

    /// One unary ControlRpc call to the host engine, 10s deadline (the engine
    /// itself caps folder listing at 6s).
    func call<Response: Decodable>(method: String, params: [String: Any]) async throws -> Response {
        try await connect()
        let id = nextId
        nextId += 1
        var frame: [String: Any] = ["id": id, "method": method]
        if !params.isEmpty { frame["params"] = params }
        let payload = try JSONSerialization.data(withJSONObject: frame)
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)

        guard let socket else { throw RelayError.notConnected }
        try await socket.send(.data(data))

        let result: Result<Data, RelayError> = await withCheckedContinuation { continuation in
            pending[id] = continuation
            Task {
                try? await Task.sleep(nanoseconds: 10_000_000_000)
                self.timeoutCall(id: id)
            }
        }
        switch result {
        case .failure(let error): throw error
        case .success(let ok):
            return try JSONDecoder().decode(Response.self, from: ok)
        }
    }

    private func timeoutCall(id: UInt64) {
        if let continuation = pending.removeValue(forKey: id) {
            continuation.resume(returning: .failure(.timeout))
        }
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message) {
        switch message {
        case .string:
            return  // "pong"
        case .data(let data):
            guard let (header, payload) = Self.decodeFrame(data) else { return }
            switch header.k {
            case Self.rpcKind:
                handleRpcPayload(payload)
            case Self.relayKind:
                // {"error":"host_offline"|"host_closed"|...} — link down.
                teardown(error: .hostOffline)
            default:
                return
            }
        @unknown default:
            return
        }
    }

    private func handleRpcPayload(_ payload: Data) {
        // ndjson: each line is one ServerFrame.
        guard let text = String(data: payload, encoding: .utf8) else { return }
        for line in text.split(separator: "\n") {
            guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                  let id = (obj["id"] as? NSNumber)?.uint64Value,
                  let continuation = pending.removeValue(forKey: id) else { continue }
            if let err = obj["err"] as? String {
                continuation.resume(returning: .failure(.rpc(err)))
            } else if obj.keys.contains("ok"),
                      let okData = try? JSONSerialization.data(withJSONObject: obj["ok"] ?? NSNull(),
                                                               options: .fragmentsAllowed) {
                continuation.resume(returning: .success(okData))
            } else {
                continuation.resume(returning: .failure(.rpc("unexpected reply")))
            }
        }
    }

    // MARK: Frame codec

    struct FrameHeader: Decodable {
        var s: String?
        var k: String?
        var to: String?
        var from: String?
    }

    static func encodeFrame(header: String, payload: Data) -> Data {
        let headerBytes = Data(header.utf8)
        var out = Data()
        var len = UInt64(headerBytes.count)
        repeat {
            var byte = UInt8(len & 0x7f)
            len >>= 7
            if len != 0 { byte |= 0x80 }
            out.append(byte)
        } while len != 0
        out.append(headerBytes)
        out.append(payload)
        return out
    }

    static func decodeFrame(_ data: Data) -> (FrameHeader, Data)? {
        var offset = 0
        var length: UInt64 = 0
        var shift: UInt64 = 0
        let bytes = [UInt8](data)
        while offset < bytes.count {
            let byte = bytes[offset]
            offset += 1
            length |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { break }
            shift += 7
            if shift > 28 { return nil }
        }
        guard offset + Int(length) <= bytes.count else { return nil }
        let headerData = Data(bytes[offset..<offset + Int(length)])
        guard let header = try? JSONDecoder().decode(FrameHeader.self, from: headerData) else { return nil }
        let payload = Data(bytes[(offset + Int(length))...])
        return (header, payload)
    }
}
