// Block-granularity transcript with a single owner for follow intent.
// UIKit owns row reuse and scrolling; SwiftUI owns message presentation.
import SwiftUI

struct TranscriptView: View {
    let store: SessionStore
    let chatId: String
    let scroll: ScrollState

    static let maxContentWidth: CGFloat = 736
    static let stickThreshold: CGFloat = 70
    static let jumpThreshold: CGFloat = 140
    @State private var veils = VeilStore()
    @State private var folds: [String: Bool] = [:]
    @State private var userExpansionHeights: [String: CGFloat] = [:]
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.verticalSizeClass) private var verticalSizeClass

    var body: some View {
        let rows = store.transcriptCache.rows(revision: store.revision,
                                              entries: store.entries,
                                              pendingSends: store.pendingSends)
        let runway = store.lastSubmittedMessageId
        NativeTranscriptTable(rows: rows, scroll: scroll, runwayID: runway,
            expansionHeight: runway.flatMap { userExpansionHeights[$0] } ?? 0,
            bottomSpacing: verticalSizeClass == .compact ? 8 : 24,
            reduceMotion: reduceMotion,
            configurationID: folds.hashValue ^ store.expandedUserMessages.hashValue ^ dynamicTypeSize.hashValue) { row in
                AnyView(rowView(row)
                    .modifier(TranscriptTailProbe(rowID: row.id,
                        isTail: row.id == rows.last?.id || (row.entryId == runway && row.turnStart),
                        chatId: chatId))
                    .environment(\.dynamicTypeSize, dynamicTypeSize)
                    .environment(\.colorScheme, .dark))
            }
            .modifier(TranscriptViewportProbe(chatId: chatId))
            .background(Theme.bg)
            .overlay(alignment: .bottomTrailing) {
                if scroll.showJump {
                    Button {
                        scroll.arm()
                        scroll.jumpToLatest?(!reduceMotion)
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

    @ViewBuilder
    private func rowView(_ row: TranscriptRow) -> some View {
        Group {
            switch row.kind {
            case .user(let text):
                UserBubble(text: text, pending: row.timestamp == nil,
                           deviceId: store.hostDeviceId ?? "",
                           expanded: store.expandedUserMessages.contains(row.entryId),
                           onToggle: {
                    // Expanding a prompt is an explicit reading action.
                    // Keep its visible position instead of chasing the tail.
                    scroll.pinned = false
                    if !store.expandedUserMessages.insert(row.entryId).inserted {
                        store.expandedUserMessages.remove(row.entryId)
                    }
                    scroll.refreshLayout?()
                }, onExpansionHeightChanged: { height in
                    guard (userExpansionHeights[row.entryId] ?? 0) != height else { return }
                    userExpansionHeights[row.entryId] = height
                    scroll.refreshLayout?()
                })
            case .markdown(let block, let streaming):
                MarkdownRowView(row: row, block: block, streaming: streaming, veils: veils)
            case .toolGroup(let tools, let autoOpen):
                ToolGroupView(tools: tools, open: folds[row.id] ?? autoOpen,
                              userToggled: folds[row.id] != nil, toggle: {
                    withAnimation(reduceMotion ? nil : Motion.resize) {
                        folds[row.id] = !(folds[row.id] ?? autoOpen)
                    }
                }, onDetailChanged: { scroll.refreshLayout?() })
            case .inputChip(let header, let resolved):
                InputChipView(header: header, resolved: resolved)
            case .errorChip(let message):
                ErrorChipView(message: message)
            }
        }
        .padding(.top, row.topGap)
        .padding(.horizontal, 20)
        .frame(maxWidth: Self.maxContentWidth)
        .frame(maxWidth: .infinity)
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
    @ObservationIgnored weak var nativeScrollView: UIScrollView?
    @ObservationIgnored var refreshLayout: (() -> Void)?
    @ObservationIgnored var jumpToLatest: ((Bool) -> Void)?
    @ObservationIgnored var interactionEpoch: UInt64 = 0
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

/// Keep each row's fade clock across hosted-cell reconfiguration. A replaced
/// hosting view disappearing must not discard the replacement's active fade.
@Observable
final class VeilStore {
    @ObservationIgnored private var veils: [String: RowVeil] = [:]

    func veil(for rowId: String, seededLength: Int) -> RowVeil {
        if let existing = veils[rowId] {
            // Register the delta before Text is constructed. Doing this in
            // onChange paints one opaque frame, then makes the same words dark.
            existing.noteLength(seededLength)
            return existing
        }
        let veil = RowVeil(seededLength: seededLength)
        veils[rowId] = veil
        return veil
    }
}

// MARK: - User bubble (transcript.rs:1671)

struct UserBubble: View {
    let text: String
    var pending = false
    /// The chat's host device — where attachment files live (read-back key).
    var deviceId = ""
    var expanded = false
    var onToggle: () -> Void = {}
    var onExpansionHeightChanged: (CGFloat) -> Void = { _ in }
    @State private var fullHeight: CGFloat = 0
    @State private var collapsedHeight: CGFloat = 0

    static func needsCollapse(_ text: String) -> Bool {
        text.count > 400 || text.split(separator: "\n", omittingEmptySubsequences: false).count > 5
    }

    private func prompt(_ text: String) -> some View {
        Text(text)
            .font(Theme.sans(MD.textSize))
            .lineSpacing(MD.lineHeight - MD.textSize - 4)
            .foregroundStyle(Theme.text)
    }

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
                let collapsible = Self.needsCollapse(parsed.text) || fullHeight > collapsedHeight + 0.5
                VStack(alignment: .leading, spacing: 0) {
                    prompt(parsed.text)
                        .lineLimit(expanded ? nil : 5)
                        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height in
                            if !expanded { collapsedHeight = height }
                        }
                        .background(alignment: .topLeading) {
                            if expanded {
                                prompt(parsed.text)
                                    .lineLimit(5)
                                    .fixedSize(horizontal: false, vertical: true)
                                    .hidden()
                                    .accessibilityHidden(true)
                                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { collapsedHeight = $0 }
                            }
                        }
                        .background(alignment: .topLeading) {
                            prompt(parsed.text)
                                .fixedSize(horizontal: false, vertical: true)
                                .hidden()
                                .accessibilityHidden(true)
                                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { fullHeight = $0 }
                        }
                    if collapsible {
                        Button {
                            onToggle()
                        } label: {
                            Text(expanded ? "Show less" : "Show more")
                                .font(Theme.sans(14))
                                .foregroundStyle(Theme.textMuted)
                                .frame(minHeight: 44)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
                    }
                }
                .onChange(of: expanded && collapsedHeight > 0 ? max(0, fullHeight - collapsedHeight) : 0, initial: true) { _, height in
                    onExpansionHeightChanged(height)
                }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius, style: .continuous))
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
        // Jump-button visibility can animate the enclosing transcript update.
        // Do not let that tween sweep the disclosure through revealed text.
        .transaction { $0.animation = nil }
    }
}

// MARK: - Markdown row with veil

struct MarkdownRowView: View {
    let row: TranscriptRow
    let block: MDBlock
    let streaming: Bool
    let veils: VeilStore

    @State private var fading = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var textLength: Int {
        switch block {
        case .paragraph(let runs), .heading(_, let runs): return runs.reduce(0) { $0 + $1.text.count }
        default: return 0
        }
    }

    var body: some View {
        if streaming, isVeilable, !reduceMotion {
            // Reattaching a row must never fade already-visible text to black.
            let veil = veils.veil(for: row.id, seededLength: textLength)
            TimelineView(.animation(minimumInterval: 1.0 / 60, paused: !fading)) { _ in
                veiledText(veil: veil)
            }
            .onChange(of: textLength) { _, _ in
                fading = veil.isFading
            }
            .task(id: textLength) {
                // Stop frame work when the last appended span has settled.
                // A new chunk cancels this task and starts a new deadline.
                try? await Task.sleep(for: .milliseconds(600))
                while !Task.isCancelled, veil.isFading {
                    try? await Task.sleep(for: .milliseconds(100))
                }
                guard !Task.isCancelled else { return }
                fading = false
            }
            .onAppear { fading = veil.isFading }
            .transaction { $0.animation = nil }
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
    private func veiledText(veil: RowVeil) -> some View {
        switch block {
        case .paragraph(let runs):
            runs.styledVeiled(veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .heading(let level, let runs):
            let m = MD.headingMetrics(level)
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
