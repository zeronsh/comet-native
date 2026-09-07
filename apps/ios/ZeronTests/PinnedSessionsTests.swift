import XCTest
@testable import Zeron

final class PinnedSessionsTests: XCTestCase {
    private func chat(_ id: String, activity: Int64, archived: Bool = false) -> Chat {
        Chat(id: id, deviceId: "device", title: id, archived: archived,
             cwd: nil, branch: nil, checkoutId: nil, config: nil,
             lastMessagePreview: nil, lastMessageAt: activity, createdAt: activity,
             spaceId: "space", lastSeenAt: activity)
    }

    func testPinsLeadInSharedOrderWithoutDisturbingRecency() {
        let chats = [chat("recent", activity: 30), chat("middle", activity: 20),
                     chat("old", activity: 10)]
        XCTAssertEqual(
            sortPinnedFirst(chats, pinnedSessionIds: ["old", "missing", "old"])
                .map(\.id),
            ["old", "recent", "middle"]
        )
    }

    func testPreferencesFieldRoundTripsThroughRegistryOverlay() {
        let doc = RegistryDoc(deviceId: "ios-test")
        doc.write(kind: "preferences", id: "sidebar-v1", op: .upsert, set: [
            "pinnedSessionIds": .array([.string("chat-b"), .string("chat-a")]),
        ])
        guard case .array(let values)? = doc.overlayRow(
            kind: "preferences", id: "sidebar-v1"
        )?.fields["pinnedSessionIds"] else {
            return XCTFail("missing sidebar preferences overlay")
        }
        XCTAssertEqual(values.compactMap(\.stringValue), ["chat-b", "chat-a"])
    }
}
