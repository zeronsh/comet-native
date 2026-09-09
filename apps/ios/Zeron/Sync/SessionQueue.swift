// The session doc's pending-message queue, from the phone's side
// (crates/doc/src/queue.rs).
//
// Unlike `messages` — host-only — every device may write here, so these are
// plain doc edits for enqueue and reorder. Protected edits and removal ask
// the host to serialize with delivery. Local list changes sync out
// with the next update. A MovableList because reordering is a real move op:
// two devices dragging at once converge on one order with every row intact,
// where a delete+insert list would duplicate or lose rows.
//
// Delivery, protected editing and cancellation are host-authoritative.
// A local projection only removes a row after a successful host ACK.

import Foundation
import Loro

extension SessionStore {
    // MARK: Read

    nonisolated static func queuedFrom(_ value: LoroValue) -> QueuedMessage? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue?.trimmingCharacters(in: .whitespaces), !id.isEmpty,
              let text = m["text"]?.stringValue,
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }
        return QueuedMessage(
            id: id,
            text: text,
            attachments: (m["attachments"]?.listValue ?? []).compactMap(\.stringValue),
            issuedBy: m["issuedBy"]?.stringValue ?? "",
            issuedAt: m["issuedAt"]?.i64Value ?? 0,
            editedAt: m["editedAt"]?.i64Value,
            holdForTurnEnd: m["holdForTurnEnd"]?.boolValue ?? false,
            deliveryGate: queueDeliveryGate(from: m["deliveryGate"])
        )
    }

    nonisolated private static func queueDeliveryGate(from value: LoroValue?) -> QueueDeliveryGate? {
        guard let gate = value?.mapValue, let kind = gate["kind"]?.stringValue else { return nil }
        let owner = gate["ownerDeviceId"]?.stringValue ?? "another device"
        switch kind {
        case "editing":
            return .editing(ownerDeviceId: owner,
                            expiresAtMs: gate["expiresAtMs"]?.i64Value ?? 0)
        case "reviewRequired":
            return .reviewRequired(ownerDeviceId: owner)
        default:
            // Unknown future gates fail closed in the UI too.
            return .reviewRequired(ownerDeviceId: owner)
        }
    }

    // MARK: Write

    /// Park a message on the queue. The host decides where it goes from there —
    /// straight into the running turn, or the front of the next one.
    @discardableResult
    func enqueueMessage(text: String, attachments: [String] = [],
                        holdForTurnEnd: Bool = true) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        let id = UUID().uuidString.lowercased()
        let list = doc.getMovableList(id: "queue")
        do {
            let map = try list.insertMapContainer(pos: list.len(), child: LoroMap())
            try map.insert(key: "id", v: id)
            try map.insert(key: "text", v: text)
            try map.insert(key: "issuedBy", v: deviceId)
            try map.insert(key: "issuedAt", v: nowMs())
            if holdForTurnEnd {
                try map.insert(key: "holdForTurnEnd", v: true)
            }
            if !attachments.isEmpty {
                try map.insert(key: "attachments", v: LoroValue.fromJSON(attachments))
            }
            doc.commit()
        } catch {
            return nil
        }
        refreshQueue()
        nudgeHost()
        return id
    }

    // MARK: Protected editing

    func beginQueuedEdit(id: String, instanceId: String) async -> QueueEditStartResult {
        struct Reply: Decodable {
            var outcome: String
            var leaseId: String?
            var text: String?
            var baseTextHash: String?
            var expiresAtMs: Int64?
        }
        guard !queueActionsPending.contains(id),
              queue.contains(where: { $0.id == id }), let relay = hostRelayClient() else {
            return .unavailable
        }
        do {
            let reply: Reply = try await relay.call(
                method: "BeginQueuedMessageEdit",
                params: [
                    "chatId": chatId,
                    "id": id,
                    "editorDeviceId": deviceId,
                    "editorInstanceId": instanceId,
                ]
            )
            switch reply.outcome {
            case "acquired":
                guard let leaseId = reply.leaseId,
                      let baseTextHash = reply.baseTextHash,
                      let expiresAtMs = reply.expiresAtMs else { return .unavailable }
                return .acquired(QueueEditLease(
                    rowId: id,
                    leaseId: leaseId,
                    text: reply.text ?? "",
                    baseTextHash: baseTextHash,
                    expiresAtMs: expiresAtMs
                ))
            case "locked": return .locked
            default: return .missing
            }
        } catch {
            roomLog.warning(
                "chat2 \(self.chatId, privacy: .public): begin queue edit failed for \(id, privacy: .public)"
            )
            return .unavailable
        }
    }

    func renewQueuedEdit(_ lease: QueueEditLease) async -> Bool {
        struct Reply: Decodable { var outcome: String }
        guard let relay = hostRelayClient() else { return false }
        do {
            let reply: Reply = try await relay.call(
                method: "RenewQueuedMessageEdit",
                params: ["chatId": chatId, "id": lease.rowId, "leaseId": lease.leaseId]
            )
            return reply.outcome == "renewed"
        } catch {
            return false
        }
    }

    func finishQueuedEdit(_ lease: QueueEditLease, action: String,
                          text: String? = nil) async -> QueueEditFinishResult {
        struct Reply: Decodable { var outcome: String }
        guard let relay = hostRelayClient() else { return .unavailable }
        var params: [String: Any] = [
            "chatId": chatId,
            "id": lease.rowId,
            "leaseId": lease.leaseId,
            "action": action,
            "expectedTextHash": lease.baseTextHash,
        ]
        if let text { params["text"] = text }
        do {
            let reply: Reply = try await relay.call(
                method: "FinishQueuedMessageEdit", params: params
            )
            switch reply.outcome {
            case "committed", "cancelled", "discarded", "released": return .finished
            case "conflict": return .conflict
            case "missing": return .missing
            default: return .lost
            }
        } catch {
            roomLog.warning(
                "chat2 \(self.chatId, privacy: .public): finish queue edit failed for \(lease.rowId, privacy: .public)"
            )
            return .unavailable
        }
    }

    /// Move a row to `to`, clamped to the queue. Same slot is a no-op.
    func moveQueued(id: String, to: Int) {
        guard !queueActionsPending.contains(id),
              queue.first(where: { $0.id == id })?.deliveryGate == nil else { return }
        let list = doc.getMovableList(id: "queue")
        let count = Int(list.len())
        guard count > 0, let from = queueIndex(of: id, in: list) else { return }
        let target = min(max(to, 0), count - 1)
        guard from != target else { return }
        do {
            try list.mov(from: UInt32(from), to: UInt32(target))
            doc.commit()
        } catch { return }
        refreshQueue()
    }

    /// Nudge a row one slot up (-1) or down (+1).
    func moveQueued(id: String, by direction: Int) {
        guard let from = queue.firstIndex(where: { $0.id == id }),
              let to = MessageQueue.neighbour(of: from, direction: direction, count: queue.count)
        else { return }
        moveQueued(id: id, to: to)
    }

    /// The host serializes these actions with delivery. Never delete locally
    /// before its ACK; a lost reply is uncertain and leaves sync to reconcile.
    @discardableResult
    func performQueueAction(id: String, action: QueueAction,
                            call: ((String, [String: String]) async throws -> QueueActionReply)? = nil) async -> Bool {
        guard let row = queue.first(where: { $0.id == id }),
              !queueActionsPending.contains(id) else { return false }
        guard action == .remove || row.deliveryGate == nil else { return false }
        queueActionsPending.insert(id)
        queueActionError = nil
        defer { queueActionsPending.remove(id) }
        do {
            let params = ["chatId": chatId, "id": id]
            let reply: QueueActionReply
            if let call {
                reply = try await call(action.method, params)
            } else {
                guard let relay = hostRelayClient() else { throw RelayError.hostOffline }
                reply = try await relay.call(method: action.method, params: params)
            }
            guard reply.acknowledged(action) else {
                queueActionError = "The host did not confirm the action. The message may have already left the queue."
                kickRoom()
                return false
            }
            // Apply only a confirmed removal. Sends leave projection to sync.
            if action == .remove {
                let list = doc.getMovableList(id: "queue")
                if let index = queueIndex(of: id, in: list) {
                    try list.delete(pos: UInt32(index), len: 1)
                    doc.commit()
                    refreshQueue()
                }
            }
            return true
        } catch {
            roomLog.warning("queue action \(action.method, privacy: .public) failed for \(id, privacy: .public): \(error.localizedDescription, privacy: .public)")
            queueActionError = "Couldn't complete \(action.label.lowercased()). Check the connection to the chat host and the queue before retrying."
            kickRoom()
            return false
        }
    }

    // MARK: Plumbing

    private func queueIndex(of id: String, in list: LoroMovableList) -> Int? {
        (0..<Int(list.len())).first { i in
            list.get(index: UInt32(i))?.asLoroMap()?.get(key: "id")?.asValue()?.stringValue == id
        }
    }

}
