// Transcript — virtualized block-granularity rows with stick-to-bottom.
//
// Desktop parity (transcript.rs): GAP_TURN 14 / GAP_BLOCK 8 / MD_BLOCK_GAP 12,
// content column max 736, re-engage band 70, jump-button threshold 320,
// bottom pad 24. Rows are identified by stable ids and versioned by content
// fingerprints, so a streamed token re-renders exactly one row. SwiftUI's lazy
// stack + scroll APIs stand in for gpui's list(): the pin breaks only on
// user scroll-up and re-engages when approaching the bottom.

import SwiftUI

struct TranscriptView: View {
    let store: SessionStore
    let chatId: String

    static let gapTurn: CGFloat = 14
    static let gapBlock: CGFloat = 8
    static let maxContentWidth: CGFloat = 736
    static let stickThreshold: CGFloat = 70
    static let jumpThreshold: CGFloat = 320

    @State private var builder = TranscriptBuilderCache()
    @State private var veils = VeilStore()
    @State private var folds: [String: Bool] = [:]
    @State private var pinned = true
    @State private var distanceFromBottom: CGFloat = 0
    @State private var userScrolling = false
    @State private var scrollPosition = ScrollPosition(edge: .bottom)
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let rows = builder.rows(entries: store.entries, pendingSends: store.pendingSends)
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(Array(rows.enumerated()), id: \.element.id) { ix, row in
                    rowView(row, previous: ix > 0 ? rows[ix - 1] : nil, isFirst: ix == 0)
                        .id(row.id)
                }
                Color.clear.frame(height: 44)  // bottom pad clears the fade + floating status strip
            }
            .frame(maxWidth: Self.maxContentWidth)
            .frame(maxWidth: .infinity)
        }
        .scrollPosition($scrollPosition)
        .defaultScrollAnchor(.bottom)
        .background(Theme.bg)
        .task {
            // Preloaded transcripts (disk hydration, demo) exist at first
            // layout, and lazy row materialization drifts the default bottom
            // anchor — snap once the first pass settles.
            try? await Task.sleep(nanoseconds: 80_000_000)
            scrollPosition.scrollTo(edge: .bottom)
        }
        .onScrollPhaseChange { _, newPhase in
            // Desktop rule: the pin breaks only on USER input (wheel-up/drag),
            // never on streaming growth. Phases track the gesture.
            userScrolling = newPhase == .interacting || newPhase == .decelerating
        }
        .onScrollGeometryChange(for: CGFloat.self) { geo in
            max(0, geo.contentSize.height + geo.contentInsets.bottom - geo.containerSize.height - geo.contentOffset.y)
        } action: { old, new in
            distanceFromBottom = new
            if userScrolling, new > old + 1, new > 2 {
                pinned = false
            } else if !pinned, new <= Self.stickThreshold, new < old {
                // Re-stick only when moving TOWARD the bottom inside the 70pt
                // band, else the pin would be unbreakable.
                pinned = true
            }
        }
        .onChange(of: contentSignature(rows)) {
            guard pinned else { return }
            if reduceMotion {
                scrollPosition.scrollTo(edge: .bottom)
            } else {
                withAnimation(.spring(duration: 0.3)) {
                    scrollPosition.scrollTo(edge: .bottom)
                }
            }
        }
        .overlay(alignment: .top) {
            // Soft fade under the nav bar — content dissolves instead of
            // hard-clipping against the header.
            LinearGradient(
                stops: [
                    .init(color: Theme.bg, location: 0),
                    .init(color: Theme.bg.opacity(0.85), location: 0.45),
                    .init(color: Theme.bg.opacity(0), location: 1),
                ],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: 130)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
        }
        .overlay(alignment: .bottom) {
            // Short ramp that reaches FULL bg at the bottom edge — content
            // dissolves completely beneath the floating status strip, but the
            // fade starts low enough that message bottoms stay legible.
            LinearGradient(
                stops: [
                    .init(color: Theme.bg.opacity(0), location: 0),
                    .init(color: Theme.bg.opacity(0.55), location: 0.45),
                    .init(color: Theme.bg, location: 0.9),
                    .init(color: Theme.bg, location: 1),
                ],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: 44)
            .allowsHitTesting(false)
        }
        .overlay(alignment: .bottomTrailing) {
            // Jump-to-bottom floats ABOVE the fades.
            if distanceFromBottom > Self.jumpThreshold {
                Button {
                    pinned = true
                    withAnimation(.spring(duration: 0.35)) {
                        scrollPosition.scrollTo(edge: .bottom)
                    }
                } label: {
                    Image(systemName: "arrow.down")
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .frame(width: 36, height: 36)
                }
                .glassEffect(.regular.interactive(), in: Circle())
                .padding(.trailing, 16)
                .padding(.bottom, 12)
                .transition(.opacity.combined(with: .move(edge: .bottom)))
            }
        }
        .motionAnimation(Motion.fadeQuick, value: distanceFromBottom > Self.jumpThreshold)
    }

    // Streamed growth signature: last row id + version + count. Any append or
    // reflow of the tail bumps it; scroll-back through history doesn't.
    private func contentSignature(_ rows: [TranscriptRow]) -> String {
        guard let last = rows.last else { return "" }
        return "\(rows.count)|\(last.id)|\(last.version)"
    }

    // MARK: Row rendering

    @ViewBuilder
    private func rowView(_ row: TranscriptRow, previous: TranscriptRow?, isFirst: Bool) -> some View {
        let gap: CGFloat = isFirst
            ? Self.gapTurn + 10
            : row.turnStart ? Self.gapTurn
            : sameMarkdownPart(row, previous) ? MD.blockGap
            : Self.gapBlock

        Group {
            switch row.kind {
            case .user(let text):
                UserBubble(text: text, pending: row.timestamp == nil)

            case .markdown(let block, let streaming):
                MarkdownRowView(row: row, block: block, streaming: streaming, veils: veils)

            case .toolGroup(let tools, let autoOpen):
                ToolGroupView(tools: tools,
                              open: folds[row.id] ?? autoOpen,
                              userToggled: folds[row.id] != nil) {
                    withAnimation(reduceMotion ? nil : Motion.resize) {
                        folds[row.id] = !(folds[row.id] ?? autoOpen)
                    }
                }

            case .inputChip(let header, let resolved):
                InputChipView(header: header, resolved: resolved)

            case .errorChip(let message):
                ErrorChipView(message: message)
            }
        }
        .padding(.top, gap)
        .padding(.horizontal, 16)
    }

    private func sameMarkdownPart(_ row: TranscriptRow, _ previous: TranscriptRow?) -> Bool {
        guard let previous, case .markdown = row.kind, case .markdown = previous.kind else { return false }
        // Ids are "{entry}#{part}.{ix}" — same prefix ⇒ same part.
        return row.id.split(separator: ".").dropLast().joined() ==
            previous.id.split(separator: ".").dropLast().joined()
    }
}

/// Row-build cache: one incremental parser per streaming part, reused across
/// body evaluations (a reference type, so building rows never mutates state
/// mid-render).
final class TranscriptBuilderCache {
    private var parsers: [String: IncrementalMarkdownParser] = [:]

    func rows(entries: [MessageEntry],
              pendingSends: [(messageId: String, text: String, at: Int64)]) -> [TranscriptRow] {
        TranscriptRowBuilder.rows(entries: entries, pendingSends: pendingSends, parsers: &parsers)
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

    var body: some View {
        HStack {
            Spacer(minLength: 0)
            Text(text)
                .font(Theme.sans(MD.textSize))
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
                .frame(maxWidth: TranscriptView.maxContentWidth * 0.8, alignment: .trailing)
                .opacity(pending ? 0.65 : 1)
                .contextMenu {
                    Button {
                        UIPasteboard.general.string = text
                    } label: {
                        Label("Copy", systemImage: "doc.on.doc")
                    }
                }
        }
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

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header stays quiet even on failure — chips carry the red.
            Button(action: toggle) {
                HStack(spacing: 8) {
                    Text(open ? "▾" : "▸")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 18, height: 18)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 5))
                    Text(toolGroupSummary(tools))
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .frame(height: 26)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if open {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(tools.enumerated()), id: \.offset) { _, tool in
                        ToolChipRow(tool: tool)
                    }
                }
                .padding(.top, 2)
            }
        }
    }
}

/// 38pt row containing a 30pt card (transcript.rs tool_chip).
struct ToolChipRow: View {
    let tool: ToolItem

    var body: some View {
        HStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: tool.call.chipSymbol)
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textMuted)
                    .frame(width: 18, height: 18)
                    .background(whiteAlpha(0.08), in: RoundedRectangle(cornerRadius: 5))
                Text(tool.call.chipLabel)
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.textMuted)
                Text(tool.call.chipDetail)
                    .font(Theme.sans(12))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.text.opacity(0.85))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 30)
            .background(whiteAlpha(0.03), in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
            .padding(.leading, 12)
        }
        .frame(height: 38)
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
