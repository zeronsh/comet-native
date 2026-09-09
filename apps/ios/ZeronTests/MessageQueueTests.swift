// The pending-message queue on the session doc, from the phone's side.

import Loro
import XCTest
import UIKit
@testable import Zeron

@MainActor
final class MessageQueueTests: XCTestCase {
    func testEmptyQueueEditPreservesAttachmentsAndDiscardsOnlyTextOnlyRows() {
        XCTAssertEqual(MessageQueue.editedText("  \n", hasAttachments: true), attachmentOnlyText)
        XCTAssertNil(MessageQueue.editedText("  \n", hasAttachments: false))
        XCTAssertEqual(MessageQueue.editedText(" updated ", hasAttachments: true), "updated")
        XCTAssertEqual(MessageQueue.editedText(" updated ", hasAttachments: false), "updated")
    }

    func testTerminalEditFailuresRetainBothDraftsAndAllowLocalRecovery() {
        let lease = QueueEditLease(rowId: "row", leaseId: "lease", text: "queued",
                                   baseTextHash: "hash", expiresAtMs: 60_000)
        for result in [QueueEditFinishResult.missing, .lost] {
            var edit = QueueComposerEdit(lease: lease, originalDraft: "unsent draft", hasAttachments: true)
            edit.receive(result)
            XCTAssertTrue(edit.terminal)
            XCTAssertEqual(edit.originalDraft, "unsent draft")
            XCTAssertEqual(edit.textToCommit("edited text"), "edited text")
            XCTAssertEqual(edit.textToCommit(""), attachmentOnlyText)
            edit.receive(.unavailable)
            XCTAssertTrue(edit.terminal, "a later network failure must not remove the local escape")
        }
    }

    func testRetryableEditFailuresKeepProtectionAndAttachmentPolicy() {
        let lease = QueueEditLease(rowId: "row", leaseId: "lease", text: "queued",
                                   baseTextHash: "hash", expiresAtMs: 60_000)
        var edit = QueueComposerEdit(lease: lease, originalDraft: "draft", hasAttachments: false)
        for result in [QueueEditFinishResult.conflict, .unavailable] {
            edit.receive(result)
            XCTAssertFalse(edit.terminal)
            XCTAssertEqual(edit.lease, lease)
            XCTAssertEqual(edit.originalDraft, "draft")
        }
        XCTAssertNil(edit.textToCommit(""))
    }

    private func store() -> SessionStore {
        SessionStore(chatId: "chat-1", config: AppConfig(
            edgeURL: URL(string: "https://example.test")!,
            mode: .dev, userId: "u1", orgId: "o1",
            deviceId: "ios-test", deviceName: "Dan’s iPhone",
            devBearer: "cmt_dev_test"))
    }

    private func texts(_ store: SessionStore) -> [String] {
        store.queue.map(\.text)
    }

    func testEnqueueStampsTheRowAndShowsItImmediately() {
        let store = store()
        XCTAssertTrue(store.queue.isEmpty)
        let id = store.enqueueMessage(text: "check the logs", attachments: ["uploads/a.png"])
        XCTAssertNotNil(id)
        XCTAssertEqual(store.queue.count, 1)
        let row = store.queue[0]
        XCTAssertEqual(row.text, "check the logs")
        XCTAssertEqual(row.attachments, ["uploads/a.png"])
        XCTAssertEqual(row.issuedBy, "ios-test")
        XCTAssertGreaterThan(row.issuedAt, 0)
        XCTAssertNil(row.editedAt)
        XCTAssertTrue(row.holdForTurnEnd)
        // Nothing to send is not a queue row.
        XCTAssertNil(store.enqueueMessage(text: "   "))
        XCTAssertEqual(store.queue.count, 1)
    }

    func testMovingByArrowsAndToAnIndex() {
        let store = store()
        let ids = ["a", "b", "c"].compactMap { store.enqueueMessage(text: $0) }
        XCTAssertEqual(ids.count, 3)

        store.moveQueued(id: ids[2], by: -1)
        XCTAssertEqual(texts(store), ["a", "c", "b"])
        // Already at the top: nothing moves.
        store.moveQueued(id: ids[0], by: -1)
        XCTAssertEqual(texts(store), ["a", "c", "b"])
        // A drop past the end lands at the back rather than failing.
        store.moveQueued(id: ids[0], to: 99)
        XCTAssertEqual(texts(store), ["c", "b", "a"])
    }

    func testRemovalWaitsForHostAcknowledgementAndSuppressesCompetingActions() async throws {
        let store = store()
        let id = try XCTUnwrap(store.enqueueMessage(text: "first"))
        store.enqueueMessage(text: "second")
        let removed = await store.performQueueAction(id: id, action: .remove) { method, params in
            XCTAssertEqual(method, "RemoveQueuedMessage")
            XCTAssertEqual(params, ["chatId": "chat-1", "id": id])
            XCTAssertEqual(self.texts(store), ["first", "second"], "keep the row until ACK")
            XCTAssertTrue(store.queueActionsPending.contains(id))
            store.moveQueued(id: id, to: 1)
            XCTAssertEqual(self.texts(store), ["first", "second"])
            let duplicate = await store.performQueueAction(id: id, action: .sendNow) { _, _ in
                XCTFail("an in-flight removal must prevent a competing send")
                return QueueActionReply(sent: true)
            }
            XCTAssertFalse(duplicate)
            return QueueActionReply(removed: true)
        }
        XCTAssertTrue(removed)
        XCTAssertEqual(texts(store), ["second"])
        XCTAssertTrue(store.queueActionsPending.isEmpty)
    }

    func testFailedAndMalformedRemovalKeepTheRowAndSurfaceError() async throws {
        let store = store()
        let id = try XCTUnwrap(store.enqueueMessage(text: "keep me"))
        for reply in [QueueActionReply(removed: false), QueueActionReply(sent: true)] {
            let removed = await store.performQueueAction(id: id, action: .remove) { _, _ in reply }
            XCTAssertFalse(removed)
            XCTAssertEqual(texts(store), ["keep me"])
            XCTAssertNotNil(store.queueActionError)
        }
        let removed = await store.performQueueAction(id: id, action: .remove) { _, _ in
            throw RelayError.hostOffline
        }
        XCTAssertFalse(removed)
        XCTAssertEqual(texts(store), ["keep me"])
        XCTAssertNotNil(store.queueActionError)
        XCTAssertTrue(store.queueActionsPending.isEmpty)
    }

    func testSendNowUsesHostMethodAndLeavesProjectionToSync() async throws {
        let store = store()
        let id = try XCTUnwrap(store.enqueueMessage(text: "hello"))
        let delivered = await store.performQueueAction(id: id, action: .sendNow) { method, _ in
            XCTAssertEqual(method, "SendQueuedMessageNow")
            return QueueActionReply(sent: true)
        }
        XCTAssertTrue(delivered)
        XCTAssertEqual(texts(store), ["hello"])
        XCTAssertNil(store.queueActionError)
        let sent = await store.performQueueAction(id: id, action: .sendNow) { _, _ in
            throw RelayError.hostOffline
        }
        XCTAssertFalse(sent)
        XCTAssertNotNil(store.queueActionError)
    }

    func testPrimaryActionRespectsCapabilitiesAttachmentsAndDeliveryGates() {
        var item = QueuedMessage(id: "q", text: "hello")
        func action(supported: Bool = true, pending: Bool = false) -> QueueAction? {
            MessageQueue.primaryAction(for: item,
                                       supportsActions: supported, pending: pending)
        }
        XCTAssertEqual(action(), .sendNow)
        XCTAssertNil(action(supported: false))
        XCTAssertNil(action(pending: true))
        item.attachments = ["uploads/image.png"]
        XCTAssertEqual(action(), .sendNow)
        item.deliveryGate = .reviewRequired(ownerDeviceId: "mac")
        XCTAssertNil(action())
        item.deliveryGate = .editing(ownerDeviceId: "mac", expiresAtMs: 60_000)
        XCTAssertNil(action())
    }

    func testProtectedRowsCannotBeDeliveredEvenThroughDirectStoreCall() async throws {
        let store = store()
        let id = try XCTUnwrap(store.enqueueMessage(text: "protected"))
        let map = try XCTUnwrap(store.doc.getMovableList(id: "queue").get(index: 0)?.asLoroMap())
        try map.insert(key: "deliveryGate", v: LoroValue.fromJSON([
            "kind": "reviewRequired", "ownerDeviceId": "mac"
        ]))
        store.doc.commit()
        store.refreshQueue()
        let sent = await store.performQueueAction(id: id, action: .sendNow) { _, _ in
            XCTFail("protected rows must not dispatch")
            return QueueActionReply(sent: true)
        }
        XCTAssertFalse(sent)
        XCTAssertEqual(texts(store), ["protected"])
    }

    func testExternalKeyboardCommandInvokesSubmitWithoutChangingText() throws {
        let view = ComposerTextView()
        view.text = "draft"
        var submitted = false
        view.modifiedSubmit = { submitted = true }
        let command = try XCTUnwrap(view.keyCommands?.first {
            $0.input == "\r" && $0.modifierFlags == .command
        })
        XCTAssertTrue(command.wantsPriorityOverSystemBehavior)
        view.perform(command.action, with: command)
        XCTAssertTrue(submitted)
        XCTAssertEqual(view.text, "draft")
    }

    func testHarnessMustAdvertiseMidTurnSteering() {
        XCTAssertNil(HarnessInfo(id: "codex", label: "Codex").midTurnSteering)
        XCTAssertEqual(HarnessInfo(id: "codex", label: "Codex", supportsSteering: true,
                                   steeringMode: "step-boundary").midTurnSteering, true)
        XCTAssertEqual(HarnessInfo(id: "other", label: "Other", supportsSteering: true,
                                   steeringMode: "turn-boundary").midTurnSteering, false)
    }

    /// The Mac's rows land here — same container, same field names.
    func testRowsWrittenByAnotherDeviceDecode() {
        let store = store()
        let list = store.doc.getMovableList(id: "queue")
        let map = try! list.insertMapContainer(pos: 0, child: LoroMap())
        try! map.insert(key: "id", v: "q-desktop")
        try! map.insert(key: "text", v: "from the Mac")
        try! map.insert(key: "issuedBy", v: "desktop")
        try! map.insert(key: "issuedAt", v: Int64(1_700_000_000_000))
        try! map.insert(key: "editedAt", v: Int64(1_700_000_000_500))
        store.doc.commit()
        store.refreshQueue()

        XCTAssertEqual(store.queue.count, 1)
        XCTAssertEqual(store.queue[0].id, "q-desktop")
        XCTAssertEqual(store.queue[0].issuedBy, "desktop")
        XCTAssertEqual(store.queue[0].editedAt, 1_700_000_000_500)
        // Old Desktop rows omitted the field and remain steerable.
        XCTAssertFalse(store.queue[0].holdForTurnEnd)
    }

    func testHoldForTurnEndRoundTripsAndCanBeDisabled() {
        let store = store()
        _ = store.enqueueMessage(text: "next turn")
        _ = store.enqueueMessage(text: "steer if possible", holdForTurnEnd: false)

        XCTAssertEqual(store.queue.map(\.holdForTurnEnd), [true, false])
    }

    func testDeliveryGatesDecodeAndUnknownKindsFailClosed() {
        let store = store()
        let list = store.doc.getMovableList(id: "queue")
        for (index, kind) in ["editing", "futureGate"].enumerated() {
            let map = try! list.insertMapContainer(pos: UInt32(index), child: LoroMap())
            try! map.insert(key: "id", v: "q-\(index)")
            try! map.insert(key: "text", v: "message \(index)")
            try! map.insert(key: "issuedBy", v: "desktop")
            try! map.insert(key: "issuedAt", v: Int64(1_000))
            try! map.insert(key: "deliveryGate", v: LoroValue.fromJSON([
                "kind": kind,
                "ownerDeviceId": "desktop",
                "expiresAtMs": Int64(60_000),
            ]))
        }
        store.doc.commit()
        store.refreshQueue()

        XCTAssertEqual(
            store.queue[0].deliveryGate,
            .editing(ownerDeviceId: "desktop", expiresAtMs: 60_000)
        )
        XCTAssertEqual(
            store.queue[1].deliveryGate,
            .reviewRequired(ownerDeviceId: "desktop")
        )
    }

    func testLabelsAndNeighbours() {
        XCTAssertNil(MessageQueue.label(0))
        XCTAssertEqual(MessageQueue.label(1), "1 queued")
        XCTAssertEqual(MessageQueue.label(4), "4 queued")
        XCTAssertEqual(MessageQueue.oneLine(" run the\n tests  now "), "run the tests now")
        XCTAssertEqual(MessageQueue.neighbour(of: 1, direction: -1, count: 3), 0)
        XCTAssertNil(MessageQueue.neighbour(of: 0, direction: -1, count: 3))
        XCTAssertNil(MessageQueue.neighbour(of: 2, direction: 1, count: 3))
    }

    func testLegacyAttachmentTrailerIsNotExposedAsQueueText() {
        let paths = ["/tmp/image.png"]
        let legacy = withAttachments(text: "inspect this", paths: paths)
        XCTAssertEqual(
            MessageQueue.visibleText(legacy, attachments: paths),
            "inspect this"
        )

        let imageOnly = withAttachments(text: "", paths: paths)
        XCTAssertEqual(
            MessageQueue.visibleText(imageOnly, attachments: paths),
            attachmentOnlyText
        )
        XCTAssertEqual(
            MessageQueue.visibleText("literal user text", attachments: paths),
            "literal user text"
        )
    }
}
