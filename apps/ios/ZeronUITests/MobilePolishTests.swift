import XCTest

@MainActor
final class MobilePolishTests: XCTestCase {
    private var app: XCUIApplication!

    override func setUp() {
        continueAfterFailure = false
        app = XCUIApplication()
        XCUIDevice.shared.orientation = .portrait
    }

    private func launch(_ arguments: [String] = []) {
        app.launchArguments = ["-demo"] + arguments
        app.launch()
    }

    private func capture(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    private func waitUntilHittable(_ element: XCUIElement, timeout: TimeInterval = 5) {
        let ready = XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: element)
        XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: timeout), .completed)
    }

    private var composer: XCUIElement { app.descendants(matching: .any)["composer-input"].firstMatch }
    private var tail: XCUIElement {
        app.otherElements["transcript"].cells["a599#t1.0"].staticTexts.firstMatch
    }

    func testImmediateScrollAfterRepeatedSessionOpens() {
        launch(["-route", "chat:chat-tabs", "-huge"])
        let transcript = app.otherElements["transcript"]
        for cycle in 0..<3 {
            if cycle > 0 {
                app.navigationBars.buttons.firstMatch.tap()
                app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Tool group header colors")).firstMatch.tap()
            }
            for direction in [1.0, -1.0, 1.0, -1.0] {
                let start = transcript.coordinate(withNormalizedOffset: CGVector(dx: 0.55, dy: direction > 0 ? 0.3 : 0.8))
                let end = transcript.coordinate(withNormalizedOffset: CGVector(dx: 0.55, dy: direction > 0 ? 0.8 : 0.3))
                start.press(forDuration: 0.01, thenDragTo: end, withVelocity: .fast, thenHoldForDuration: 0)
                let viewport = transcript.frame
                let visible = transcript.staticTexts.allElementsBoundByIndex.filter {
                    $0.frame.intersects(viewport) && $0.frame.height > 0
                }
                XCTAssertGreaterThan(visible.map { min($0.frame.maxY, viewport.maxY) }.max() ?? 0,
                                     viewport.midY, "A scroll must not leave the lower half of the transcript blank")
            }
            capture("early-scroll-\(cycle)")
        }
    }

    func testLongUserMessageShowMoreAndLess() {
        launch(["-route", "chat:chat-tabs", "-longprompt"])
        let more = app.buttons["Show more"]
        XCTAssertTrue(more.waitForExistence(timeout: 5))
        capture("user-message-collapsed")
        more.tap()
        let less = app.buttons["Show less"]
        capture("user-message-expanded")
        for _ in 0..<5 where !less.isHittable { app.otherElements["transcript"].swipeUp() }
        waitUntilHittable(less)
        less.tap()
        waitUntilHittable(more)
        capture("user-message-recollapsed")
        more.tap()
        app.navigationBars.buttons.firstMatch.tap()
        app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Tool group header colors")).firstMatch.tap()
        waitUntilHittable(less)
        capture("user-message-expanded-reopen")
        less.tap()
        waitUntilHittable(more)
    }

    func testStreamingComposerAndCancelledBackSwipeKeepTranscriptVisible() {
        launch(["-route", "chat:chat-tabs", "-slowstream"])
        composer.tap()
        composer.typeText("Check keyboard and navigation transitions.")
        app.buttons["composer-send"].tap()
        let reply = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "Here’s how")).firstMatch
        XCTAssertTrue(reply.waitForExistence(timeout: 8))
        func visibleReply() -> XCUIElement? {
            app.otherElements["transcript"].staticTexts.allElementsBoundByIndex.last {
                $0.label != "Check keyboard and navigation transitions." && $0.isHittable
            }
        }
        func waitForVisibleReply() {
            let visible = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                visibleReply() != nil
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [visible], timeout: 5), .completed)
        }
        for cycle in 0..<3 {
            waitUntilHittable(app.keyboards.firstMatch)
            waitForVisibleReply()
            capture("stream-keyboard-\(cycle)")
            visibleReply()!.tap()
            // A slow, short edge swipe is cancelled, returning to this session.
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.005, dy: 0.5))
                .press(forDuration: 0.05,
                       thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.22, dy: 0.5)),
                       withVelocity: .slow, thenHoldForDuration: 0.4)
            XCTAssertTrue(composer.exists)
            waitForVisibleReply()
            capture("cancelled-back-swipe-\(cycle)")
            composer.tap()
            waitForVisibleReply()
        }
    }

    func testSendClearsComposerAndKeepsTheNextDraft() {
        launch(["-route", "chat:chat-tabs", "-slowstream"])
        composer.tap()
        for (cycle, prompt) in ["Fix teh flicker", "A multiline draft\nwith another line", "Keep emoji 👋🏽 and 你好"].enumerated() {
            composer.typeText(prompt)
            XCTAssertEqual(composer.value as? String, prompt)
            app.buttons["composer-send"].tap()
            let cleared = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                (self.composer.value as? String ?? "").isEmpty
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [cleared], timeout: 3), .completed)
            waitUntilHittable(app.keyboards.firstMatch)
            let next = "Next draft \(cycle)"
            composer.typeText(next)
            XCTAssertEqual(composer.value as? String, next)
            capture("sent-cleared-next-draft-\(cycle)")
            composer.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: next.count))
        }
    }

    func testGlassProjectSelectorSelectionAndDismissal() {
        launch()
        let filter = app.buttons["space-filter"]
        XCTAssertTrue(filter.waitForExistence(timeout: 5))
        for (index, project) in ["zeron", "edge", "All", "zeron", "All"].enumerated() {
            filter.tap()
            if index == 0 { capture("project-menu") }
            let choice = app.buttons.matching(NSPredicate(format: "label == %@", project)).firstMatch
            XCTAssertTrue(choice.waitForExistence(timeout: 3))
            choice.tap()
            XCTAssertTrue(filter.waitForExistence(timeout: 3))
            capture("filter-selection-\(index)-\(project)")
            filter.tap()
            app.tapCoordinate(x: 0.8, y: 0.7)
            XCTAssertTrue(filter.exists)
            capture("filter-dismiss-\(index)")
        }
    }

    func testHugeTranscriptKeyboardFoldsAndWarmReopen() {
        launch(["-route", "chat:chat-tabs", "-huge"])
        XCTAssertTrue(tail.waitForExistence(timeout: 8))
        waitUntilHittable(tail)
        capture("huge-transcript")
        composer.tap()
        XCTAssertTrue(app.buttons["Attach photos"].waitForExistence(timeout: 3))
        XCTAssertLessThanOrEqual(tail.frame.maxY, composer.frame.minY)
        capture("huge-keyboard")
        composer.typeText("A multiline draft\nwith a second line\nand a third line\nand another line to grow the composer.")
        waitUntilHittable(tail)
        XCTAssertLessThanOrEqual(tail.frame.maxY, composer.frame.minY)
        capture("multiline-composer")
        // Read history and return with the real scroll gesture / jump button.
        tail.tap() // Dismiss keyboard before measuring a history drag.
        let transcript = app.otherElements["transcript"]
        for _ in 0..<2 {
            transcript.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.35))
                .press(forDuration: 0.05, thenDragTo: transcript.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.85)))
        }
        let jump = app.buttons["jump-to-latest"]
        XCTAssertTrue(jump.waitForExistence(timeout: 3))
        jump.tap()
        waitUntilHittable(tail)
        capture("jump-restored")
        for index in 0..<3 {
            app.navigationBars.buttons.firstMatch.tap()
            let session = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Tool group header colors")).firstMatch
            XCTAssertTrue(session.waitForExistence(timeout: 3))
            session.tap()
            XCTAssertTrue(tail.waitForExistence(timeout: 5))
            waitUntilHittable(tail)
            capture("warm-reopen-\(index)")
        }
    }

    func testDelayedHydrationWithoutScrolling() {
        launch(["-route", "chat:chat-tabs", "-huge", "-hydrate-late"])
        XCTAssertTrue(tail.waitForExistence(timeout: 8))
        waitUntilHittable(tail)
        capture("delayed-hydration")
    }

    func testToolGroupRepeatedOpenAndClose() {
        launch(["-route", "chat:chat-tabs"])
        let group = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Ran 1 command")).firstMatch
        XCTAssertTrue(group.waitForExistence(timeout: 5))
        let detail = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "cargo test")).firstMatch
        for cycle in 0..<3 {
            group.tap()
            XCTAssertEqual(group.value as? String, "Expanded")
            XCTAssertTrue(detail.exists)
            if cycle == 0 { capture("tool-group-expanded") }
            group.tap()
            XCTAssertEqual(group.value as? String, "Collapsed")
            let removed = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: detail)
            XCTAssertEqual(XCTWaiter.wait(for: [removed], timeout: 2), .completed)
            if cycle == 0 { capture("tool-group-collapsed") }
        }
        XCTAssertTrue(composer.isHittable)
    }

    func testToolRailAndComposerPicker() {
        launch(["-route", "chat:chat-tabs"])
        let group = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Ran 1 command")).firstMatch
        XCTAssertTrue(group.waitForExistence(timeout: 5))
        group.tap()
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "cargo test")).firstMatch.exists)
        capture("tool-activity")
        composer.tap()
        let model = app.buttons["GPT-5.6-Terra"]
        XCTAssertTrue(model.waitForExistence(timeout: 3))
        model.tap()
        capture("model-picker")
        app.swipeDown()
        XCTAssertTrue(composer.waitForExistence(timeout: 3))
        capture("composer-after-picker")
    }

    func testQuestionPanelAndNewSessionFlow() {
        launch(["-route", "chat:chat-picker"])
        let customAnswer = app.textFields["Or type your own answer"]
        XCTAssertTrue(customAnswer.waitForExistence(timeout: 5))
        capture("question-panel")
        customAnswer.tap()
        customAnswer.typeText("Use the owning device.")
        XCTAssertTrue(app.buttons["Submit"].isHittable)
        capture("question-keyboard")
        app.navigationBars.buttons.firstMatch.tap()
        let create = app.buttons["New session"]
        XCTAssertTrue(create.waitForExistence(timeout: 3))
        create.tap()
        app.buttons.matching(NSPredicate(format: "label == %@", "zeron")).firstMatch.tap()
        XCTAssertTrue(app.buttons["composer-send"].waitForExistence(timeout: 3))
        capture("new-session")
    }

    func testLocalSendRunwayAndLandscape() {
        launch(["-route", "chat:chat-tabs"])
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap()
        composer.typeText("Please verify the mobile layout.")
        app.buttons["composer-send"].tap()
        let reply = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "Here’s how the streamed reply")).firstMatch
        XCTAssertTrue(reply.waitForExistence(timeout: 8))
        capture("runway-streaming")
        let completedTail = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "When the turn settles")).firstMatch
        XCTAssertTrue(completedTail.waitForExistence(timeout: 10))
        XCUIDevice.shared.orientation = .landscapeLeft
        let landscape = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            self.app.frame.width > self.app.frame.height
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [landscape], timeout: 5), .completed)
        Thread.sleep(forTimeInterval: 1) // Let the keyboard finish its rotation animation.
        waitUntilHittable(composer)
        let send = app.buttons["composer-send"]
        waitUntilHittable(send)
        XCTAssertLessThanOrEqual(send.frame.maxY, app.keyboards.firstMatch.frame.minY + 2)
        XCTAssertGreaterThan(app.otherElements["transcript"].frame.height, 20)
        XCTAssertGreaterThan(completedTail.frame.maxY, app.otherElements["transcript"].frame.minY)
        XCTAssertLessThanOrEqual(completedTail.frame.maxY, app.otherElements["transcript"].frame.maxY + 2)
        capture("landscape-streaming")
        XCUIDevice.shared.orientation = .portrait
        let portrait = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            self.app.frame.height > self.app.frame.width
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [portrait], timeout: 5), .completed)
        Thread.sleep(forTimeInterval: 1)
        waitUntilHittable(composer)
        capture("portrait-restored")
    }
}

private extension XCUIApplication {
    func tapCoordinate(x: CGFloat, y: CGFloat) {
        coordinate(withNormalizedOffset: CGVector(dx: x, dy: y)).tap()
    }
}
