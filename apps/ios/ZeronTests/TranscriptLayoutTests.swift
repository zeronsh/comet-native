#if DEBUG
import SwiftUI
import XCTest
@testable import Zeron

@MainActor
final class TranscriptLayoutTests: XCTestCase {
    @Observable final class Harness {
        let store: SessionStore
        var scroll = ScrollState()
        var identity = UUID()
        var composerHeight: CGFloat = 64
        var dynamicTypeSize: DynamicTypeSize = .large
        init(store: SessionStore) { self.store = store }
    }

    private struct Surface: View {
        let harness: Harness
        var body: some View {
            VStack(spacing: 0) {
                TranscriptView(store: harness.store, chatId: harness.store.chatId, scroll: harness.scroll)
                    .id(harness.identity)
                Color.black.frame(height: harness.composerHeight)
            }
            .environment(\.dynamicTypeSize, harness.dynamicTypeSize)
        }
    }

    private var window: UIWindow!
    private var harness: Harness!
    private var key: String { harness.store.chatId }

    private func mount(turns: Int, size: CGSize = CGSize(width: 390, height: 844)) async {
        TranscriptLayoutProbe.enabled = true
        let config = AppConfig(edgeURL: URL(string: "http://localhost:8787")!, mode: .dev,
                               userId: "test", orgId: "test", deviceId: "test", deviceName: "Test")
        let store = SessionStore(chatId: UUID().uuidString, config: config, offline: true)
        store.setEntries(BenchRunner.syntheticEntries(turns: turns))
        harness = Harness(store: store)
        let scene = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first!
        window = UIWindow(windowScene: scene)
        window.frame = CGRect(origin: .zero, size: size)
        window.rootViewController = UIHostingController(rootView: Surface(harness: harness)
            .frame(width: size.width, height: size.height))
        window.makeKeyAndVisible()
        await settle()
    }

    private func settle() async {
        // Let real SwiftUI/UIKit layout and lazy measurement passes run.
        try? await Task.sleep(for: .milliseconds(700))
        window.layoutIfNeeded()
    }

    private func assertTailVisible(file: StaticString = #filePath, line: UInt = #line) {
        let lastRow = harness.store.transcriptCache.rows(revision: harness.store.revision,
            entries: harness.store.entries, pendingSends: harness.store.pendingSends).last!
        guard let tail = TranscriptLayoutProbe.tails[key + "|" + lastRow.id],
              let viewport = TranscriptLayoutProbe.viewports[key] else {
            let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
            }
            let attachment = XCTAttachment(image: image)
            attachment.lifetime = .keepAlways
            add(attachment)
            XCTFail("Tail \(lastRow.id) must be realized without a user scroll; distance \(harness.scroll.distanceFromBottom)", file: file, line: line)
            return
        }
        if tail.maxY < viewport.minY {
            let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
            }
            let attachment = XCTAttachment(image: image)
            attachment.lifetime = .keepAlways
            add(attachment)
        }
        XCTAssertGreaterThan(tail.height, 0, file: file, line: line)
        XCTAssertGreaterThan(tail.maxY, viewport.minY, file: file, line: line)
        XCTAssertLessThanOrEqual(tail.maxY, viewport.maxY + 2, file: file, line: line)
        XCTAssertLessThan(viewport.maxY - tail.maxY, 40, file: file, line: line)
        XCTAssertTrue(harness.scroll.pinned, file: file, line: line)
    }

    override func tearDown() {
        window?.isHidden = true
        window?.rootViewController = nil
        window = nil
        harness = nil
        TranscriptLayoutProbe.tails.removeAll()
        TranscriptLayoutProbe.viewports.removeAll()
        TranscriptLayoutProbe.enabled = false
        super.tearDown()
    }

    func testWarm600TurnOpenRealizesTailWithoutScroll() async {
        await mount(turns: 600)
        assertTailVisible()
    }

    func testDelayed600TurnHydrationRealizesTailWithoutScroll() async {
        await mount(turns: 0)
        harness.store.setEntries(BenchRunner.syntheticEntries(turns: 600))
        await settle()
        assertTailVisible()
    }

    func testRepeatedComposerAndKeyboardResizesKeepTailVisible() async {
        await mount(turns: 120)
        for height: CGFloat in [144, 440, 200, 64, 440, 64] {
            harness.composerHeight = height
            await settle()
            assertTailVisible()
        }
    }

    func testLargeTailShrinkClampsWithoutUserScroll() async {
        await mount(turns: 600)
        harness.store.setEntries(BenchRunner.syntheticEntries(turns: 2))
        await settle()
        assertTailVisible()
    }

    func testStreamingAppendKeepsPinnedTailVisible() async {
        await mount(turns: 10)
        for turns in 11...14 {
            harness.store.setEntries(BenchRunner.syntheticEntries(turns: turns))
            await settle()
            assertTailVisible()
        }
    }

    func testCompactLandscapeViewportStillRealizesTail() async {
        await mount(turns: 120, size: CGSize(width: 844, height: 390))
        assertTailVisible()
    }

    func testAccessibilityTextResizeKeepsTailVisible() async {
        await mount(turns: 120)
        harness.dynamicTypeSize = .accessibility3
        await settle()
        assertTailVisible()
        harness.dynamicTypeSize = .large
        await settle()
        assertTailVisible()
    }

    func testNarrowViewportKeepsTailVisible() async {
        await mount(turns: 120, size: CGSize(width: 320, height: 568))
        assertTailVisible()
    }

    func testLocalRunwaySurvivesCompletionAndResizeThenHandsOffToLongReply() async {
        await mount(turns: 2)
        let store = harness.store
        store.demoResponder = { [weak store] prompt in
            guard let store else { return }
            var entries = store.entries
            entries.append(MessageEntry(id: "local-prompt", role: .user,
                parts: [.text(id: "t0", text: prompt)], createdAt: nowMs(),
                deviceId: "test", status: .complete, continuationOf: nil))
            entries.append(MessageEntry(id: "local-reply", role: .assistant,
                parts: [.text(id: "t0", text: "A short completed reply.")], createdAt: nowMs(),
                deviceId: "test", status: .complete, continuationOf: nil))
            store.setEntries(entries)
        }
        store.sendSteer(prompt: "Inspect this layout.")
        await settle()
        for height: CGFloat in [64, 440, 64] {
            harness.composerHeight = height
            await settle()
            let prompt = TranscriptLayoutProbe.tails[key + "|local-prompt"]!
            let viewport = TranscriptLayoutProbe.viewports[key]!
            XCTAssertEqual(prompt.minY, viewport.minY, accuracy: 3)
        }
        var entries = store.entries
        entries[entries.count - 1].parts = [.text(id: "t0", text:
            Array(repeating: "A long response consumes the reserved space and continues following the tail.", count: 30).joined(separator: "\n\n"))]
        store.setEntries(entries)
        await settle()
        assertTailVisible()
    }

    func testRepeatedWarmReopensResetReleasedFollow() async {
        await mount(turns: 600)
        for _ in 0..<5 {
            harness.scroll.pinned = false
            harness.identity = UUID()
            harness.scroll = ScrollState()
            await settle()
            assertTailVisible()
        }
    }
    func testSingleLongTurnCrossesEagerTailWindow() async {
        await mount(turns: 1)
        for paragraphs in [30, 80, 120] {
            var entries = harness.store.entries
            entries[entries.count - 1].parts = [.text(id: "long", text:
                (0..<paragraphs).map { "Paragraph \($0): a long streamed response must keep its newest block visible." }.joined(separator: "\n\n"))]
            harness.store.setEntries(entries)
            await settle()
            assertTailVisible()
        }
    }

}
#endif
