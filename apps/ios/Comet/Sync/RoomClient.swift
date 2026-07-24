// Loro room client — a Swift port of crates/sync/src/room.rs.
//
// One client per room (workspace doc or session doc), one WebSocket carrying
// two sub-rooms: the `%LOR` doc room and the `%EPH` presence room. The client
// joins with its local oplog VV, imports the server's backfill, resubmits
// anything the server lacks (covers unacked updates across reconnects), and
// relays local commits as DocUpdate batches until acked.

import Foundation
import Loro

enum RoomEvent {
    case connected
    case disconnected
    case remoteUpdate
    case ephemeralUpdate
}

actor RoomClient {
    // Constants mirrored from room.rs.
    static let fragmentBytes = 200_000
    static let pingIntervalNs: UInt64 = 30_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    static let backoffBaseMs = 250
    static let backoffCapMs = 30_000
    static let maxInvalidRejoins = 3
    static let maxFragmentCount: UInt64 = 4096
    static let maxReassembledBytes = 64 * 1024 * 1024

    let roomId: String
    let doc: LoroDoc
    let eph: EphemeralStore
    private let urlProvider: @Sendable () async -> URL?
    private let events: @Sendable (RoomEvent) -> Void

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var pending: [BatchId: [[UInt8]]] = [:]
    private var fragments: [BatchId: FragmentBuffer] = [:]
    private var joinedLor = false
    private var invalidRejoins = 0
    private var fullResyncRequested = false
    private var backoffMs = RoomClient.backoffBaseMs
    private var lastInbound = DispatchTime.now()
    private var closed = false
    private var generation = 0

    private struct FragmentBuffer {
        var crdt: CrdtType
        var parts: [[UInt8]?]
        var received: Int
        var totalSize: Int
    }

    init(roomId: String,
         doc: LoroDoc,
         ephTimeoutMs: Int64 = 30_000,
         urlProvider: @escaping @Sendable () async -> URL?,
         events: @escaping @Sendable (RoomEvent) -> Void) {
        self.roomId = roomId
        self.doc = doc
        self.eph = EphemeralStore(timeout: ephTimeoutMs)
        self.urlProvider = urlProvider
        self.events = events
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        connect()
    }

    func stop() {
        closed = true
        generation += 1
        receiveTask?.cancel()
        pingTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joinedLor = false
    }

    private func connect() {
        guard !closed else { return }
        generation += 1
        let gen = generation
        joinedLor = false
        fullResyncRequested = false
        fragments.removeAll()

        Task {
            guard let url = await urlProvider() else {
                await self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) {
        guard gen == generation, !closed else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()

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
                try? await Task.sleep(nanoseconds: RoomClient.pingIntervalNs)
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        // Join the doc room with our local VV (empty VV asks for a snapshot).
        Task { await self.sendJoinLoro(version: self.localVersionBytes()) }
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) {
        guard gen == generation, !closed else { return }
        events(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed else { return }
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        receiveTask?.cancel()
        pingTask?.cancel()
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, RoomClient.backoffCapMs)
        Task {
            try? await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000)
            await self.connect()
        }
    }

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > RoomClient.silenceLeaseNs {
            onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        switch message {
        case .string:
            return  // "pong" — lease already refreshed
        case .data(let data):
            guard let frame = LoroWire.decode(data) else { return }
            await handleFrame(frame, gen: gen)
        @unknown default:
            return
        }
    }

    private func handleFrame(_ frame: ProtocolMessage, gen: Int) async {
        switch frame {
        case .joinResponseOk(let crdt, _, _, let version, _):
            await onJoinOk(crdt: crdt, version: version)

        case .joinError(let crdt, _, let code, _):
            if crdt == .loro {
                if code == .versionUnknown {
                    // Server can't diff from our VV — full snapshot backfill.
                    await sendJoinLoro(version: [])
                } else {
                    // AuthFailed / AppError: back off and retry (token refresh
                    // may fix it on the next dial).
                    onSocketError(gen: gen)
                }
            }

        case .docUpdate(_, _, let updates, _):
            applyRemote(crdt: frameCrdt(frame), updates: updates)

        case .docUpdateFragmentHeader(let crdt, _, let batchId, let count, let total):
            guard count > 0, count <= RoomClient.maxFragmentCount,
                  total <= UInt64(RoomClient.maxReassembledBytes) else { return }
            fragments[batchId] = FragmentBuffer(crdt: crdt, parts: Array(repeating: nil, count: Int(count)),
                                                received: 0, totalSize: Int(total))

        case .docUpdateFragment(_, _, let batchId, let index, let fragment):
            onFragment(batchId: batchId, index: Int(index), fragment: fragment)

        case .ack(let crdt, _, let refId, let status):
            await onAck(crdt: crdt, refId: refId, status: status)

        case .roomError(_, _, let code, _):
            if code == .evicted {
                onSocketError(gen: gen)
            } else {
                await sendJoinLoro(version: localVersionBytes())
            }

        case .joinRequest, .leave:
            return
        }
    }

    private func frameCrdt(_ frame: ProtocolMessage) -> CrdtType {
        if case .docUpdate(let crdt, _, _, _) = frame { return crdt }
        return .loro
    }

    private func onJoinOk(crdt: CrdtType, version: [UInt8]) async {
        switch crdt {
        case .loro:
            joinedLor = true
            backoffMs = RoomClient.backoffBaseMs
            // Resubmit-from-VV: push everything the server lacks.
            if !doc.oplogVv().isEmpty(), invalidRejoins < RoomClient.maxInvalidRejoins {
                let serverVv: VersionVector
                if version.isEmpty {
                    serverVv = VersionVector()
                } else {
                    serverVv = (try? VersionVector.decode(bytes: Data(version))) ?? VersionVector()
                }
                if let missing = try? doc.export(mode: .updates(from: serverVv)), !missing.isEmpty {
                    await sendLoroUpdates([[UInt8](missing)])
                }
            }
            // Join presence once the doc room is up.
            await send(.joinRequest(crdt: .loroEphemeral, roomId: roomId, auth: [], version: []))
            events(.connected)
        case .loroEphemeral:
            let all = eph.encodeAll()
            if !all.isEmpty {
                await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId,
                                      updates: [[UInt8](all)], batchId: .random()))
            }
        }
    }

    private func applyRemote(crdt: CrdtType, updates: [[UInt8]]) {
        switch crdt {
        case .loro:
            var imported = false
            for update in updates where !update.isEmpty {
                if let _ = try? doc.importWith(bytes: Data(update), origin: "remote") {
                    imported = true
                } else if !fullResyncRequested {
                    fullResyncRequested = true
                    Task { await self.sendJoinLoro(version: []) }
                }
            }
            if imported { events(.remoteUpdate) }
        case .loroEphemeral:
            var applied = false
            for update in updates where !update.isEmpty {
                if (try? eph.apply(data: Data(update))) != nil { applied = true }
            }
            if applied { events(.ephemeralUpdate) }
        }
    }

    private func onFragment(batchId: BatchId, index: Int, fragment: [UInt8]) {
        guard var buffer = fragments[batchId] else { return }
        guard index < buffer.parts.count else {
            fragments.removeValue(forKey: batchId)
            return
        }
        if buffer.parts[index] == nil { buffer.received += 1 }
        buffer.parts[index] = fragment
        if buffer.received < buffer.parts.count {
            fragments[batchId] = buffer
            return
        }
        fragments.removeValue(forKey: batchId)
        var total: [UInt8] = []
        total.reserveCapacity(buffer.totalSize)
        for part in buffer.parts { total.append(contentsOf: part ?? []) }
        applyRemote(crdt: buffer.crdt, updates: [total])
    }

    private func onAck(crdt: CrdtType, refId: BatchId, status: UpdateStatusCode) async {
        switch status {
        case .ok:
            pending.removeValue(forKey: refId)
        case .fragmentTimeout:
            // DO hibernated mid-batch — resend the whole batch.
            if let batch = pending.removeValue(forKey: refId) {
                await sendLoroUpdates(batch)
            }
        case .invalidUpdate, .permissionDenied:
            pending.removeValue(forKey: refId)
            if crdt == .loro, invalidRejoins < RoomClient.maxInvalidRejoins {
                invalidRejoins += 1
                await sendJoinLoro(version: localVersionBytes())
            }
        default:
            pending.removeValue(forKey: refId)
        }
    }

    // MARK: Outbound

    /// Called by the doc store on local commit (subscribeLocalUpdate bytes).
    func sendLocalUpdate(_ update: [UInt8]) async {
        guard joinedLor else { return }  // resubmit-from-VV covers pre-join commits
        await sendLoroUpdates([update])
    }

    /// Broadcast the presence store's local delta.
    func sendEphemeralUpdate(_ update: [UInt8]) async {
        guard joinedLor, !update.isEmpty else { return }
        await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId, updates: [update], batchId: .random()))
    }

    private func sendJoinLoro(version: [UInt8]) async {
        await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [], version: version))
    }

    /// Batch small updates, fragment any single update above the payload budget.
    private func sendLoroUpdates(_ updates: [[UInt8]]) async {
        var small: [[UInt8]] = []
        var smallBytes = 0
        for update in updates where !update.isEmpty {
            if update.count > RoomClient.fragmentBytes {
                await sendFragmented(update)
                continue
            }
            if smallBytes + update.count > RoomClient.fragmentBytes {
                await sendBatch(small)
                small = []
                smallBytes = 0
            }
            small.append(update)
            smallBytes += update.count
        }
        if !small.isEmpty { await sendBatch(small) }
    }

    private func sendBatch(_ updates: [[UInt8]]) async {
        let batchId = BatchId.random()
        pending[batchId] = updates
        await send(.docUpdate(crdt: .loro, roomId: roomId, updates: updates, batchId: batchId))
    }

    private func sendFragmented(_ update: [UInt8]) async {
        let batchId = BatchId.random()
        pending[batchId] = [update]
        let chunks = stride(from: 0, to: update.count, by: RoomClient.fragmentBytes).map {
            Array(update[$0..<min($0 + RoomClient.fragmentBytes, update.count)])
        }
        await send(.docUpdateFragmentHeader(crdt: .loro, roomId: roomId, batchId: batchId,
                                            fragmentCount: UInt64(chunks.count),
                                            totalSizeBytes: UInt64(update.count)))
        for (ix, chunk) in chunks.enumerated() {
            await send(.docUpdateFragment(crdt: .loro, roomId: roomId, batchId: batchId,
                                          index: UInt64(ix), fragment: chunk))
        }
    }

    private func send(_ message: ProtocolMessage) async {
        guard let socket, let data = LoroWire.encode(message) else { return }
        try? await socket.send(.data(data))
    }

    private func localVersionBytes() -> [UInt8] {
        let vv = doc.oplogVv()
        return vv.isEmpty() ? [] : [UInt8](vv.encode())
    }
}

private extension VersionVector {
    func isEmpty() -> Bool {
        // An empty VV encodes to a fixed small header with no entries; the
        // cheapest reliable emptiness probe the FFI exposes is comparing
        // against a fresh VV.
        self == VersionVector()
    }
}
