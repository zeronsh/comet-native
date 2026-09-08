// Registry room client — a Swift port of crates/sync/src/registry.rs, the
// text-frame sibling of RoomClient (which still carries the Loro session
// docs). JSON text frames over one WebSocket to /registry/{orgId}/ws:
// hello/cursor handshake, push/ack for pending op batches, merged-row
// broadcasts, presence beats, probe/redial liveness, reconnect with backoff.
//
// The client owns no row semantics: everything applies through the
// WorkspaceStore's RegistryDoc on the main actor (the Swift stand-in for the
// Rust side's doc mutex). Delivery is awaited in the receive loop, so frame
// order is preserved — the server sends the rows broadcast BEFORE the ack for
// your own push (apply rows, then retire the pending batch).
//
// Liveness discipline is inherited from room.rs and its incidents: the text
// "ping" elicits a runtime auto-pong that proves NOTHING about the DO
// (2026-07-30), so room health is judged only by protocol frames — a probe
// unanswered past its deadline tears the session down for a fresh dial.

import Foundation
import os

enum RegistryEvent: Sendable {
    /// The hello's `state` answer (also a late duplicate — applying twice is
    /// harmless and simpler than special-casing).
    case state(seq: UInt64, full: Bool, gcFloor: UInt64, rows: [RegistryRow], presence: [String: Int64])
    /// Joined (or re-joined); the state above has been delivered first.
    case connected
    /// Merged full rows for every touched row (our own op may have lost LWW —
    /// the merged row is the truth to display).
    case rows(seq: UInt64, rows: [RegistryRow])
    /// Our push landed: retire the batch; `seq` advances the cursor.
    case ack(batch: String, seq: UInt64, applied: UInt64)
    /// A remote device's presence beat.
    case presence(device: String, at: Int64)
    /// The connection dropped; unacked batches become pushable again.
    case disconnected
}

actor RegistryClient {
    // Constants mirrored from crates/sync/src/registry.rs.
    static let pingIntervalNs: UInt64 = 15_000_000_000
    static let presenceIntervalNs: UInt64 = 15_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    static let helloDeadlineNs: UInt64 = 15_000_000_000
    static let probeDeadlineNs: UInt64 = 10_000_000_000
    static let probeQuietNs: UInt64 = 900_000_000_000  // 15min quiet-room probe
    static let livenessTickNs: UInt64 = 1_000_000_000
    static let backoffBaseMs = 250
    static let backoffCapMs = 16_000
    /// Stability-gated backoff reset (registry.rs STABLE_RESET): only a
    /// session that joined AND survived this long resets backoff to base.
    static let stableResetNs: UInt64 = 30_000_000_000

    /// MainActor-isolated bridge to the store's RegistryDoc.
    struct Delegate: Sendable {
        var helloCursor: @MainActor @Sendable () -> UInt64?
        var takePushable: @MainActor @Sendable () async -> [RegistryPendingBatch]
        var event: @MainActor @Sendable (RegistryEvent) async -> Void
    }

    private let device: String
    private let urlProvider: @Sendable () async -> URL?
    private let delegate: Delegate
    private let session = URLSession(configuration: .default)
    private let sendAllowed: @Sendable () -> Bool

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var presenceTask: Task<Void, Never>?
    private var livenessTask: Task<Void, Never>?
    private var joined = false
    /// When the current session joined — feeds the stability-gated reset.
    private var joinedAt: DispatchTime?
    private var closed = false
    private var generation = 0
    private var backoffMs = RegistryClient.backoffBaseMs
    /// Transport clock — pongs count, so a healthy socket never trips it.
    private var lastInbound = DispatchTime.now()
    /// Protocol clock — only real frames count (pongs prove nothing).
    private var lastProtocolRx = DispatchTime.now()
    private var helloSentAt: DispatchTime?
    private var probeSentAt: DispatchTime?

    init(device: String,
         urlProvider: @escaping @Sendable () async -> URL?,
         delegate: Delegate, sendAllowed: @escaping @Sendable () -> Bool = { true }) {
        self.device = device
        self.urlProvider = urlProvider
        self.delegate = delegate
        self.sendAllowed = sendAllowed
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        connect()
    }

    func stop() {
        session.invalidateAndCancel()
        closed = true
        generation += 1
        cancelTasks()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joined = false
    }

    /// Local writes were enqueued — push pending batches now.
    func nudge() async {
        guard joined else { return }
        await pushPending()
    }

    /// Foreground hook (the registry twin of RoomClient.kick): suspension
    /// kills the socket without running any failure path. A dead or unjoined
    /// session redials NOW on fresh backoff; a joined one gets an immediate
    /// deadline-checked probe (post-suspend sockets are half-open more often
    /// than not).
    func kick() async {
        guard !closed else { return }
        backoffMs = RegistryClient.backoffBaseMs
        if socket == nil {
            connect()
            return
        }
        if !joined {
            // A dial/handshake is already in flight on this socket — its own
            // hello deadline polices it. Redialing here killed the launch
            // dial mid-TLS every cold open (foregrounded() fires on scene
            // activation), rerunning the handshake on the busiest link and
            // doubling the spinner (NLC Edge, 2026-08-17). Only a zombie
            // socket with NO handshake pending is redialed.
            if helloSentAt == nil {
                connect()
            }
            return
        }
        guard probeSentAt == nil, helloSentAt == nil else { return }  // already policed
        await sendProbe()
    }

    private func cancelTasks() {
        receiveTask?.cancel()
        pingTask?.cancel()
        presenceTask?.cancel()
        livenessTask?.cancel()
    }

    private func connect() {
        guard !closed else { return }
        generation += 1
        let gen = generation
        joined = false
        helloSentAt = nil
        probeSentAt = nil
        lastProtocolRx = .now()

        Task {
            guard let url = await urlProvider() else {
                // No URL = no token (refresh failed or signed out) — the most
                // confusing silent failure: everything cached renders, nothing
                // syncs. Say so and back off.
                roomLog.error("registry: no socket URL (token unavailable); backing off")
                self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) async {
        guard gen == generation, !closed else { return }
        let task = session.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()
        lastProtocolRx = .now()

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.currentSocket(gen: gen) else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message, gen: gen)
                } catch {
                    await self.onSocketError(gen: gen)
                    return
                }
            }
        }

        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RegistryClient.pingIntervalNs)
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        presenceTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RegistryClient.presenceIntervalNs)
                guard let self else { return }
                await self.presenceTick(gen: gen)
            }
        }

        livenessTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RegistryClient.livenessTickNs)
                guard let self else { return }
                await self.livenessTick(gen: gen)
            }
        }

        // Hello with the persisted cursor (nil asks for full state). The
        // deadline is armed BEFORE the send — an unanswered hello must never
        // hang the session (room.rs, 2026-07-30).
        helloSentAt = .now()
        let cursor = await delegate.helloCursor()
        await send(HelloFrame(cursor: cursor, device: device))
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) async {
        guard gen == generation, !closed else { return }
        roomLog.warning("registry: session ended (joined=\(self.joined)); redialing in \(self.backoffMs)ms")
        joined = false
        await delegate.event(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed else { return }
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        cancelTasks()
        // Stability-gated reset: only a session that survived ≥30s earns a
        // fresh 250ms; a connect-and-die session keeps growing the backoff.
        if let joinedAt,
           DispatchTime.now().uptimeNanoseconds - joinedAt.uptimeNanoseconds
               >= RegistryClient.stableResetNs {
            backoffMs = RegistryClient.backoffBaseMs
        }
        joinedAt = nil
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, RegistryClient.backoffCapMs)
        Task {
            // Parked while the OS path is offline; cut short by any online
            // event (a sibling dial success, path recovery, focus probe).
            await OnlineBus.shared.waitBackoff(ms: delay)
            self.connect()
        }
    }

    // MARK: Timers

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > RegistryClient.silenceLeaseNs {
            roomLog.warning("registry: socket silent past lease; treating as dead")
            await onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    private func presenceTick(gen: Int) async {
        guard gen == generation, joined else { return }
        await send(PresenceFrame(at: nowMs()))
    }

    /// Hello answers and probes run against hard deadlines; a long-quiet but
    /// joined room gets a probe. Any protocol frame clears both deadlines.
    private func livenessTick(gen: Int) async {
        guard gen == generation, socket != nil, !closed else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        if let sent = helloSentAt, now - sent.uptimeNanoseconds > RegistryClient.helloDeadlineNs {
            roomLog.warning("registry: no state frame within deadline; room presumed wedged, redialing")
            await onSocketError(gen: gen)
            return
        }
        if let sent = probeSentAt, now - sent.uptimeNanoseconds > RegistryClient.probeDeadlineNs {
            roomLog.warning("registry: probe unanswered past deadline; redialing")
            await onSocketError(gen: gen)
            return
        }
        if joined, probeSentAt == nil, helloSentAt == nil,
           now - lastProtocolRx.uptimeNanoseconds > RegistryClient.probeQuietNs {
            await sendProbe()
            // Don't re-arm the quiet timer against the same silence.
            lastProtocolRx = .now()
        }
    }

    private func sendProbe() async {
        // Armed BEFORE the send suspends — the actor is reentrant across the
        // await, and the answer must find the deadline already set.
        probeSentAt = .now()
        await send(ProbeFrame())
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        guard case .string(let text) = message else { return }
        if text == "pong" { return }  // transport lease refreshed; proves nothing
        let frame: ServerFrame
        do {
            frame = try JSONDecoder().decode(ServerFrame.self, from: Data(text.utf8))
        } catch {
            // Protocol breakdown — same as the Rust client: redial rather
            // than run blind against a server we can't parse.
            roomLog.error("registry: unparseable frame (\(String(describing: error), privacy: .public)); redialing")
            await onSocketError(gen: gen)
            return
        }
        lastProtocolRx = .now()
        probeSentAt = nil

        switch frame {
        case .state(let seq, let full, let gcFloor, let rows, let presence):
            helloSentAt = nil
            let wasJoined = joined
            joined = true
            joinedAt = .now()
            await delegate.event(.state(seq: seq, full: full, gcFloor: gcFloor,
                                        rows: rows, presence: presence))
            if !wasJoined {
                roomLog.info("registry: joined (seq=\(seq), full=\(full), rows=\(rows.count))")
                // One recovered socket un-parks the whole fleet's backoffs.
                OnlineBus.shared.notifyOnline()
                await delegate.event(.connected)
            }
            // Anything pending (offline writes, reseeds) pushes now, and our
            // beat announces this device without waiting for the timer.
            await pushPending()
            await send(PresenceFrame(at: nowMs()))

        case .rows(let seq, let rows):
            await delegate.event(.rows(seq: seq, rows: rows))

        case .ack(let batch, let seq, let applied):
            await delegate.event(.ack(batch: batch, seq: seq, applied: applied))

        case .presence(let device, let at):
            await delegate.event(.presence(device: device, at: at))

        case .probeOk:
            break  // liveness proven; clocks already advanced above

        case .error(let code, let message):
            // Rejections are server-attributed per device; surface loudly
            // (2026-07-31: silent rejects looked exactly like a working app).
            roomLog.error("registry: server rejected a frame: \(code, privacy: .public): \(message, privacy: .public)")
        }
    }

    // MARK: Outbound

    private func pushPending() async {
        let batches = await delegate.takePushable()
        for batch in batches {
            await send(PushFrame(batch: batch.batch, ops: batch.ops))
        }
    }

    private func send(_ frame: some Encodable) async {
        guard !closed, sendAllowed(), let socket, let data = try? JSONEncoder().encode(frame),
              let text = String(data: data, encoding: .utf8) else { return }
        try? await socket.send(.string(text))
    }
}

// MARK: - Wire frames (JSON text; mirror edge/src/registry-room.ts)

private struct HelloFrame: Encodable {
    var t = "hello"
    var cursor: UInt64?
    var device: String

    // Explicit null for a nil cursor (synthesized Codable would omit the key;
    // the server treats both as "no cursor", but the wire doc says null).
    private enum CodingKeys: String, CodingKey { case t, cursor, device }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(t, forKey: .t)
        try c.encode(cursor, forKey: .cursor)
        try c.encode(device, forKey: .device)
    }
}

private struct PushFrame: Encodable {
    var t = "push"
    var batch: String
    var ops: [RegistryOp]
}

private struct PresenceFrame: Encodable {
    var t = "presence"
    var at: Int64
}

private struct ProbeFrame: Encodable {
    var t = "probe"
}

private enum ServerFrame: Decodable {
    case state(seq: UInt64, full: Bool, gcFloor: UInt64, rows: [RegistryRow], presence: [String: Int64])
    case rows(seq: UInt64, rows: [RegistryRow])
    case ack(batch: String, seq: UInt64, applied: UInt64)
    case presence(device: String, at: Int64)
    case probeOk
    case error(code: String, message: String)

    private enum Keys: String, CodingKey {
        case t, seq, full, gcFloor, rows, presence, batch, applied, device, at, code, message
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .t) {
        case "state":
            self = .state(seq: try c.decode(UInt64.self, forKey: .seq),
                          full: try c.decode(Bool.self, forKey: .full),
                          gcFloor: try c.decodeIfPresent(UInt64.self, forKey: .gcFloor) ?? 0,
                          rows: try c.decode([RegistryRow].self, forKey: .rows),
                          presence: try c.decodeIfPresent([String: Int64].self, forKey: .presence) ?? [:])
        case "rows":
            self = .rows(seq: try c.decode(UInt64.self, forKey: .seq),
                         rows: try c.decode([RegistryRow].self, forKey: .rows))
        case "ack":
            self = .ack(batch: try c.decode(String.self, forKey: .batch),
                        seq: try c.decode(UInt64.self, forKey: .seq),
                        applied: try c.decodeIfPresent(UInt64.self, forKey: .applied) ?? 0)
        case "presence":
            self = .presence(device: try c.decode(String.self, forKey: .device),
                             at: try c.decode(Int64.self, forKey: .at))
        case "probe-ok":  // the tag has a hyphen — bit the Rust side too
            self = .probeOk
        case "error":
            self = .error(code: try c.decodeIfPresent(String.self, forKey: .code) ?? "unknown",
                          message: try c.decodeIfPresent(String.self, forKey: .message) ?? "")
        case let tag:
            throw DecodingError.dataCorrupted(.init(codingPath: [Keys.t],
                                                    debugDescription: "unknown frame: \(tag)"))
        }
    }
}
