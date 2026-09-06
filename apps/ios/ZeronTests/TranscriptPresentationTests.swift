import XCTest
@testable import Zeron

@MainActor
final class TranscriptPresentationTests: XCTestCase {
    func testReattachedStreamingTextStartsVisibleAndOnlyNewTextFades() {
        let registry = VeilStore()
        let first = registry.veil(for: "row", seededLength: 80)
        XCTAssertTrue(first.segments(totalLength: 80).allSatisfy { $0.alpha == 1 })
        first.noteLength(100)
        XCTAssertEqual(first.segments(totalLength: 100).first?.alpha, 1)
        XCTAssertTrue(first.isFading)
        registry.drop("row")
        let reattached = registry.veil(for: "row", seededLength: 100)
        XCTAssertTrue(reattached.segments(totalLength: 100).allSatisfy { $0.alpha == 1 })
        XCTAssertFalse(reattached.isFading)
    }

    func testDesktopUserMessageCollapseThresholds() {
        XCTAssertFalse(UserBubble.needsCollapse("A short prompt."))
        XCTAssertFalse(UserBubble.needsCollapse(String(repeating: "x", count: 400)))
        XCTAssertTrue(UserBubble.needsCollapse(String(repeating: "x", count: 401)))
        XCTAssertTrue(UserBubble.needsCollapse((1...6).map(String.init).joined(separator: "\n")))
    }
}
