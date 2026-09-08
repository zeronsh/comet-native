import XCTest
@testable import Zeron

@MainActor
final class TranscriptFollowTests: XCTestCase {
    private func sample(_ offset: CGFloat, bottom: CGFloat = 0, height: CGFloat = 2000,
                        viewport: CGFloat = 600) -> TranscriptGeometry {
        TranscriptGeometry(contentHeight: height, viewportHeight: viewport,
                           offset: offset, bottom: bottom)
    }

    func testStreamGrowthAndKeyboardResizeDoNotReleaseFollow() {
        let state = ScrollState()
        state.observe(old: sample(1400), new: sample(1400, bottom: 180, height: 2180))
        XCTAssertTrue(state.pinned)
        state.observe(old: sample(1400, bottom: 180),
                      new: sample(1400, bottom: 480, viewport: 300))
        XCTAssertTrue(state.pinned)
        XCTAssertFalse(state.showJump)
    }

    func testTouchWithoutMovementDuringGrowthPreservesFollow() {
        let state = ScrollState()
        state.userScrolling = true
        state.observe(old: sample(1400), new: sample(1100, bottom: 300, height: 2300))
        state.endGesture()
        XCTAssertTrue(state.pinned)
    }

    func testReadingHistoryStaysReleasedDuringStreamingAndResize() {
        let state = ScrollState()
        state.userScrolling = true
        state.userDragging = true
        state.observe(old: sample(1400), new: sample(900, bottom: 500))
        state.endGesture()
        XCTAssertFalse(state.pinned)
        XCTAssertTrue(state.showJump)
        state.observe(old: sample(900, bottom: 500), new: sample(900, bottom: 800, height: 2300))
        state.observe(old: sample(900, bottom: 800), new: sample(900, bottom: 1100, viewport: 300))
        XCTAssertFalse(state.pinned)
    }

    func testMovingUpInsideReengageBandStillReleases() {
        let state = ScrollState()
        state.userScrolling = true
        state.userDragging = true
        state.observe(old: sample(1400), new: sample(1380, bottom: 20))
        state.endGesture()
        XCTAssertFalse(state.pinned)
        XCTAssertFalse(state.showJump)
    }

    func testMovingTowardBottomReengagesAndContinuesFollowing() {
        let state = ScrollState()
        state.userScrolling = true
        state.userDragging = true
        state.observe(old: sample(1400), new: sample(900, bottom: 500))
        state.observe(old: sample(900, bottom: 500), new: sample(1350, bottom: 50))
        state.endGesture()
        XCTAssertTrue(state.pinned)
        XCTAssertFalse(state.showJump)
    }

    func testExplicitJumpRearmsEvenAfterLargeHistoryScroll() {
        let state = ScrollState()
        state.userScrolling = true
        state.userDragging = true
        state.observe(old: sample(1400), new: sample(100, bottom: 1300))
        state.endGesture()
        state.arm()
        XCTAssertTrue(state.pinned)
        XCTAssertFalse(state.showJump)
    }
}
