// Block-granularity transcript with a single owner for follow intent.
// Explicit targets keep lazy height estimates out of scroll coordinates.
import SwiftUI

struct TranscriptView: View {
    let store: SessionStore
    let chatId: String
    let scroll: ScrollState

    static let maxContentWidth: CGFloat = 736
    static let stickThreshold: CGFloat = 70
    static let jumpThreshold: CGFloat = 140
    private static let bottomID = "transcript-bottom"

    @State private var veils = VeilStore()
    @State private var folds: [String: Bool] = [:]
    @State private var viewportHeight: CGFloat = 0
    @State private var runwayEntry: String?
    @State private var glideUntil = Date.distantPast
    @State private var correction: Task<Void, Never>?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    private var bottomSpacing: CGFloat { verticalSizeClass == .compact ? 8 : 24 }

    var body: some View {
        let rows = store.transcriptCache.rows(revision: store.revision,
                                              entries: store.entries,
                                              pendingSends: store.pendingSends)
        // Always realize the newest turn. A fully lazy tail can report an
        // estimated "bottom" with NO rows instantiated after a bulk append.
        // Keep history virtualized and bound eager work for very long turns.
        let ownSplit = runwayEntry.flatMap { entry in rows.firstIndex { $0.entryId == entry } }
        let latestTurn = rows.lastIndex { if case .user = $0.kind { return true }; return false } ?? 0
        let split = max(ownSplit ?? latestTurn, rows.count - 48)
        let reservesTurn = ownSplit == split && ownSplit != nil
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(rows[..<split])) { row in
                            rowView(row, proxy: proxy).id(row.id)
                        }
                    }
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(rows[split...])) { row in
                            rowView(row, proxy: proxy)
                                .modifier(TranscriptTailProbe(rowID: row.id,
                                    isTail: row.id == rows.last?.id || (row.entryId == runwayEntry && row.turnStart),
                                    chatId: chatId))
                                .id(row.id)
                        }
                        if reservesTurn { Spacer(minLength: 0) }
                    }
                    // Reply growth consumes the reservation in this layout
                    // pass. A short completed reply retains its prompt at top.
                    .frame(minHeight: reservesTurn ? max(0, viewportHeight - bottomSpacing) : 0,
                           alignment: .topLeading)
                    Color.clear.frame(height: bottomSpacing)
                        .id(Self.bottomID)
                }
                .frame(maxWidth: Self.maxContentWidth)
                .frame(maxWidth: .infinity)
            }
            .modifier(TranscriptViewportProbe(chatId: chatId))
            .accessibilityIdentifier("transcript")
            .defaultScrollAnchor(.bottom, for: .initialOffset)
            .defaultScrollAnchor(.top, for: .alignment)
            .scrollDismissesKeyboard(.interactively)
            .simultaneousGesture(TapGesture().onEnded {
                UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder),
                                                to: nil, from: nil, for: nil)
            })
            .background(Theme.bg)
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { _, height in
                viewportHeight = height
                if scroll.pinned { scheduleCorrection(proxy: proxy) }
            }
            .onScrollGeometryChange(for: TranscriptGeometry.self) { geo in
                TranscriptGeometry(contentHeight: geo.contentSize.height,
                                   viewportHeight: geo.containerSize.height,
                                   offset: geo.contentOffset.y,
                                   bottom: geo.contentSize.height - geo.visibleRect.maxY)
            } action: { old, new in
                scroll.observe(old: old, new: new)
            }
            .onScrollPhaseChange { _, phase in
                switch phase {
                case .tracking:
                    correction?.cancel()
                    scroll.userScrolling = true
                    scroll.userDragging = false
                case .interacting, .decelerating:
                    scroll.userScrolling = true
                    scroll.userDragging = true
                case .idle:
                    scroll.endGesture()
                    if scroll.pinned { scheduleCorrection(proxy: proxy) }
                case .animating: break
                @unknown default: break
                }
            }
            .task(id: rows.isEmpty) {
                guard !rows.isEmpty else { return }
                // Warm opens and delayed hydration take exactly the same path.
                // Do not erase measurements: unchanged geometry will not report again.
                for _ in 0..<12 {
                    guard !Task.isCancelled, !scroll.userScrolling, scroll.pinned else { break }
                    proxy.scrollTo(Self.bottomID, anchor: .bottom)
                    try? await Task.sleep(for: .milliseconds(30))
                    if abs(scroll.distanceFromBottom) <= 2 { break }
                }
            }
            .onChange(of: store.lastSubmittedMessageId) { _, entry in
                guard let entry else { return }
                withAnimation(reduceMotion ? nil : .spring(duration: 0.35)) {
                    runwayEntry = entry
                    scroll.arm()
                }
                scheduleCorrection(proxy: proxy, animated: true)
            }
            .onChange(of: dynamicTypeSize) {
                if scroll.pinned { scheduleCorrection(proxy: proxy) }
            }
            .onChange(of: folds) {
                if scroll.pinned { scheduleCorrection(proxy: proxy) }
            }
            .onChange(of: store.revision) {
                if let entry = runwayEntry, !rows.contains(where: { $0.entryId == entry }) {
                    runwayEntry = nil
                }
                if scroll.pinned { scheduleCorrection(proxy: proxy) }
            }
            .onDisappear {
                correction?.cancel()
            }
            .overlay(alignment: .bottomTrailing) {
                if scroll.showJump {
                    Button {
                        scroll.arm()
                        scheduleCorrection(proxy: proxy, animated: true)
                    } label: {
                        Image(systemName: "arrow.down")
                            .font(.system(size: 16, weight: .medium))
                            .foregroundStyle(Theme.text)
                            .frame(width: 44, height: 44)
                    }
                    .glassEffect(.regular.interactive(), in: Circle())
                    .accessibilityLabel("Jump to latest")
                    .accessibilityIdentifier("jump-to-latest")
                    .padding(12)
                    .transition(.opacity)
                }
            }
            .motionAnimation(Motion.fadeQuick, value: scroll.showJump)
        }
    }

    private func scheduleCorrection(proxy: ScrollViewProxy, animated: Bool = false) {
        guard scroll.pinned, !scroll.userScrolling else { return }
        // Coalesce a stream burst, but retain a trailing correction after
        // programmatic movement. No deadline that can silently drop the last one.
        correction?.cancel()
        if animated && !reduceMotion { glideUntil = Date().addingTimeInterval(0.4) }
        let delay = animated ? 0 : max(0, glideUntil.timeIntervalSinceNow)
        correction = Task { @MainActor in
            if delay > 0 { try? await Task.sleep(for: .seconds(delay)) }
            await Task.yield()
            guard !Task.isCancelled, scroll.pinned, !scroll.userScrolling else { return }
            withAnimation(animated && !reduceMotion ? .spring(duration: 0.35) : nil) {
                proxy.scrollTo(Self.bottomID, anchor: .bottom)
            }
            // A newly appended target may not exist in the reader's layout
            // map until the next pass. Retry after that pass, even when the
            // estimated content size happened to remain unchanged.
            try? await Task.sleep(for: .milliseconds(animated ? 400 : 300))
            guard !Task.isCancelled, scroll.pinned, !scroll.userScrolling else { return }
            proxy.scrollTo(Self.bottomID, anchor: .bottom)
        }
    }

    @ViewBuilder
    private func rowView(_ row: TranscriptRow, proxy: ScrollViewProxy) -> some View {
        Group {
            switch row.kind {
            case .user(let text):
                UserBubble(text: text, pending: row.timestamp == nil,
                           deviceId: store.hostDeviceId ?? "")
            case .markdown(let block, let streaming):
                MarkdownRowView(row: row, block: block, streaming: streaming, veils: veils)
            case .toolGroup(let tools, let autoOpen):
                ToolGroupView(tools: tools, open: folds[row.id] ?? autoOpen,
                              userToggled: folds[row.id] != nil, toggle: {
                    withAnimation(reduceMotion ? nil : Motion.resize) {
                        folds[row.id] = !(folds[row.id] ?? autoOpen)
                    }
                }, onDetailChanged: { scheduleCorrection(proxy: proxy) })
            case .inputChip(let header, let resolved):
                InputChipView(header: header, resolved: resolved)
            case .errorChip(let message):
                ErrorChipView(message: message)
            }
        }
        .padding(.top, row.topGap)
        .padding(.horizontal, 20)
    }
}

struct TranscriptGeometry: Equatable {
    var contentHeight: CGFloat
    var viewportHeight: CGFloat
    var offset: CGFloat
    var bottom: CGFloat
}

@Observable
final class ScrollState {
    var pinned = true
    var showJump = false
    @ObservationIgnored var userScrolling = false
    @ObservationIgnored var userDragging = false
    @ObservationIgnored var distanceFromBottom: CGFloat = 0
    @ObservationIgnored private var movedAway = false

    func observe(old: TranscriptGeometry, new: TranscriptGeometry) {
        distanceFromBottom = new.bottom
        // Offset direction, not distance direction: growing content can move
        // the bottom while the user's finger is stationary.
        if userDragging, new.offset < old.offset - 0.5 {
            pinned = false
            movedAway = true
        }
        if userDragging, new.offset > old.offset + 0.5,
           new.bottom <= TranscriptView.stickThreshold {
            pinned = true
        }
        let show = !pinned && new.bottom > TranscriptView.jumpThreshold
        if showJump != show { showJump = show }
    }

    func endGesture() {
        if userScrolling, movedAway, distanceFromBottom <= 2 { pinned = true }
        userScrolling = false
        userDragging = false
        movedAway = false
        showJump = !pinned && distanceFromBottom > TranscriptView.jumpThreshold
    }

    func arm() {
        pinned = true
        showJump = false
        movedAway = false
    }
}

/// Row-build cache: one incremental parser per streaming part plus a memo of
/// settled parses. Owned by the SessionStore (NOT view @State) so the parses
/// survive across view instances — re-opening a chat re-parses nothing.
@MainActor
final class TranscriptBuilderCache {
    private var parsers: [String: IncrementalMarkdownParser] = [:]
    private var completed: [String: CompletedParse] = [:]
    private var cachedRevision: UInt64?
    private var cachedRows: [TranscriptRow] = []
    private var prewarming = false

    /// Rows for the store's current `revision`. Rows only change when the doc
    /// does — gate on the revision and hand back the same array.
    func rows(revision: UInt64,
              entries: [MessageEntry],
              pendingSends: [PendingSend]) -> [TranscriptRow] {
        if cachedRevision == revision { return cachedRows }
        cachedRows = TranscriptRowBuilder.rows(entries: entries, pendingSends: pendingSends,
                                               parsers: &parsers, completed: &completed)
        cachedRevision = revision
        return cachedRows
    }

    /// Parse every settled part OFF the main thread and merge into the memo,
    /// so the first `rows()` of a freshly hydrated long session assembles from
    /// memo hits instead of parsing the whole transcript inside body — that
    /// synchronous parse was the "empty transcript for a while" on open.
    func prewarm(entries: [MessageEntry]) {
        guard !prewarming else { return }
        var jobs: [(key: String, text: String)] = []
        for entry in entries where entry.role != .user {
            let streaming = entry.status == .streaming
            let lastIx = entry.parts.indices.last
            for (ix, part) in entry.parts.enumerated() {
                guard case .text(let partId, let text) = part, !text.isEmpty else { continue }
                if streaming && ix == lastIx { continue }  // live tail: incremental parser's job
                let key = "\(entry.id)#\(partId)"
                if completed[key]?.source != text {
                    jobs.append((key, text))
                }
            }
        }
        guard !jobs.isEmpty else { return }
        prewarming = true
        Task { @MainActor [weak self] in
            let parsed = await Task.detached(priority: .userInitiated) {
                jobs.map { (key: $0.key, text: $0.text, blocks: MarkdownParser.parse($0.text)) }
            }.value
            guard let self else { return }
            self.prewarming = false
            for job in parsed where self.completed[job.key]?.source != job.text {
                self.completed[job.key] = CompletedParse(source: job.text, blocks: job.blocks)
            }
        }
    }
}

/// Veil registry — one RowVeil per live row, dropped on the live→complete flip.
@Observable
final class VeilStore {
    @ObservationIgnored private var veils: [String: RowVeil] = [:]

    func veil(for rowId: String, seeded: Bool) -> RowVeil {
        if let existing = veils[rowId] { return existing }
        let veil = RowVeil()
        veils[rowId] = veil
        return veil
    }

    func drop(_ rowId: String) {
        veils.removeValue(forKey: rowId)
    }
}

// MARK: - User bubble (transcript.rs:1671)

struct UserBubble: View {
    let text: String
    var pending = false
    /// The chat's host device — where attachment files live (read-back key).
    var deviceId = ""

    var body: some View {
        // Attachment refs ride the message text (message-attachments.ts
        // transport); split them out and render thumbnails above the bubble,
        // exactly like the desktop's user rows.
        let parsed = parseUserMessageImages(text)
        VStack(alignment: .trailing, spacing: 8) {
            if !parsed.attachments.isEmpty, !deviceId.isEmpty {
                UserAttachmentsStrip(deviceId: deviceId, attachments: parsed.attachments)
            }
            if !parsed.text.isEmpty {
                Text(parsed.text)
                    .font(Theme.sans(MD.textSize))
                    .lineSpacing(MD.lineHeight - MD.textSize - 4)
                    .foregroundStyle(Theme.text)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
                    .frame(maxWidth: TranscriptView.maxContentWidth * 0.8, alignment: .trailing)
                    .contextMenu {
                        Button {
                            UIPasteboard.general.string = parsed.text
                        } label: {
                            Label("Copy", systemImage: "doc.on.doc")
                        }
                    }
            }
        }
        .opacity(pending ? 0.65 : 1)
        .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

// MARK: - Markdown row with veil

struct MarkdownRowView: View {
    let row: TranscriptRow
    let block: MDBlock
    let streaming: Bool
    let veils: VeilStore

    var body: some View {
        if streaming, isVeilable {
            TimelineView(.animation) { _ in
                veiledText
            }
            .onDisappear { veils.drop(row.id) }
        } else {
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }

    private var isVeilable: Bool {
        switch block {
        case .paragraph, .heading: return true
        default: return false
        }
    }

    @ViewBuilder
    private var veiledText: some View {
        let veil = veils.veil(for: row.id, seeded: false)
        switch block {
        case .paragraph(let runs):
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .heading(let level, let runs):
            let m = MD.headingMetrics(level)
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(size: m.size, weight: .semibold, veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(m.line - m.size - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }
}

// MARK: - Tool group (transcript.rs render_tool_group)

struct ToolGroupView: View {
    let tools: [ToolItem]
    let open: Bool
    let userToggled: Bool
    let toggle: () -> Void
    var onDetailChanged: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button(action: toggle) {
                HStack(spacing: 10) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 11, weight: .semibold))
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .frame(width: 26)
                    Text(toolGroupSummary(tools))
                        .font(Theme.sans(14))
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                    Spacer(minLength: 0)
                }
                .foregroundStyle(Theme.textMuted)
                .frame(minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(PressWashButtonStyle(cornerRadius: 8))
            .accessibilityValue(open ? "Expanded" : "Collapsed")
            .accessibilityHint("Shows or hides tool activity")

            if open {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(tools.enumerated()), id: \.offset) { index, tool in
                        ToolChipRow(tool: tool, continues: index < tools.count - 1, onResize: onDetailChanged)
                    }
                }
            }
        }
    }
}

/// Desktop activity rail: quiet summary, connected icons, plain detail rows.
/// Long commands remain available through a disclosure instead of being lost
/// to middle truncation on a phone.
struct ToolChipRow: View {
    let tool: ToolItem
    var continues = false
    var onResize: () -> Void = {}
    @State private var expanded = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Button {
            withAnimation(reduceMotion ? nil : Motion.resize) { expanded.toggle() }
            onResize()
        } label: {
            HStack(alignment: .top, spacing: 10) {
                VStack(spacing: 5) {
                    Rectangle().fill(Theme.borderStrong).frame(width: 1, height: 5)
                    Image(systemName: tool.call.chipSymbol)
                        .font(.system(size: 14))
                        .foregroundStyle(tool.isError ? Theme.danger : Theme.textMuted)
                        .frame(width: 26, height: 18)
                    Rectangle().fill(continues ? Theme.borderStrong : .clear)
                        .frame(width: 1)
                }
                .frame(width: 26)
                VStack(alignment: .leading, spacing: 4) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(tool.call.chipLabel)
                            .font(Theme.sans(14, weight: .medium))
                            .foregroundStyle(tool.isError ? Theme.danger : Theme.textMuted)
                        if tool.isError {
                            Text("Failed").font(Theme.sans(12)).foregroundStyle(Theme.danger)
                        } else if !tool.resolved {
                            Text("Running").font(Theme.sans(12)).foregroundStyle(Theme.textFaint)
                        }
                    }
                    if !tool.call.chipDetail.isEmpty {
                        Text(expanded ? tool.call.expandedDetail : tool.call.chipDetail)
                            .font(Theme.mono(13))
                            .foregroundStyle(Theme.text.opacity(0.85))
                            .lineLimit(expanded ? nil : 2)
                            .multilineTextAlignment(.leading)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.vertical, 10)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .fixedSize(horizontal: false, vertical: true)
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
        .accessibilityHint("Shows the full tool details")
        .contextMenu {
            Button("Copy details", systemImage: "doc.on.doc") {
                UIPasteboard.general.string = tool.call.expandedDetail
            }
        }
    }
}

// MARK: - Chips (transcript.rs ErrorChip / InputChip)

struct ErrorChipView: View {
    let message: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 10))
                .foregroundStyle(Theme.dangerSoft.opacity(0.8))
                .frame(width: 20, height: 20)
                .background(Theme.danger.opacity(0.12), in: RoundedRectangle(cornerRadius: 6))
            Text("Error")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(message)
                .font(Theme.sans(12))
                .foregroundStyle(Theme.text.opacity(0.8))
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(Theme.danger.opacity(0.05), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.danger.opacity(0.16), lineWidth: 1))
    }
}

struct InputChipView: View {
    let header: String
    let resolved: Bool

    var body: some View {
        // Neutral throughout — resolution never recolors.
        HStack(spacing: 8) {
            Image(systemName: "bubble.left.and.text.bubble.right")
                .font(.system(size: 10))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 20, height: 20)
                .background(whiteAlpha(0.09), in: RoundedRectangle(cornerRadius: 6))
            Text("Question")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(resolved ? header : "Awaiting your answer…")
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(whiteAlpha(0.045), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(whiteAlpha(0.08), lineWidth: 1))
    }
}
