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
        let reattached = VeilStore().veil(for: "row", seededLength: 100)
        XCTAssertTrue(reattached.segments(totalLength: 100).allSatisfy { $0.alpha == 1 })
        XCTAssertFalse(reattached.isFading)
    }

    func testNewSuffixIsRegisteredBeforeItsFirstRender() {
        let registry = VeilStore()
        let initial = registry.veil(for: "row", seededLength: 5)
        let updated = registry.veil(for: "row", seededLength: 10)
        XCTAssertTrue(initial === updated)
        let segments = updated.segments(totalLength: 10)
        XCTAssertEqual(segments.first?.range, 0..<5)
        XCTAssertEqual(segments.first?.alpha, 1)
        XCTAssertEqual(segments.last?.range, 5..<10)
        XCTAssertLessThan(segments.last!.alpha, 0.1)
        XCTAssertTrue(registry.veil(for: "row", seededLength: 10) === updated)
    }

    func testVisibleSuffixNeverDimsAcrossBurstsOrMarkdownLengthChanges() {
        var now: Double = 1000
        let veil = RowVeil(seededLength: 5, clock: { now })
        veil.noteLength(10)
        var alpha: Double = 0
        for length in [10, 15, 13, 18, 25, 24, 30] {
            now += 40
            veil.noteLength(length)
            let next = veil.segments(totalLength: length).first { $0.range.contains(7) }!.alpha
            XCTAssertGreaterThanOrEqual(next, alpha)
            alpha = next
        }
        now += 2000
        XCTAssertFalse(veil.isFading)
        XCTAssertTrue(veil.segments(totalLength: 30).allSatisfy { $0.alpha == 1 })
    }

    func testDesktopUserMessageCollapseThresholds() {
        XCTAssertFalse(UserBubble.needsCollapse("A short prompt."))
        XCTAssertFalse(UserBubble.needsCollapse(String(repeating: "x", count: 400)))
        XCTAssertTrue(UserBubble.needsCollapse(String(repeating: "x", count: 401)))
        XCTAssertTrue(UserBubble.needsCollapse((1...6).map(String.init).joined(separator: "\n")))
    }
}
