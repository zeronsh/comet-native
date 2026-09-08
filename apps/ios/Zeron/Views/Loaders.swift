// Loaders + status indicators — ports of crates/ui/src/loaders.rs.
//
// gradient-spin-pulse: a 3×3 cell grid with per-row "sunrise" tints; each cell
// pulses once per 750ms with phase = distance from bottom-center, so the wave
// travels upward. The mini variant (2×3) snakes clockwise around the perimeter
// and marks Working rows in lists.

import SwiftUI

enum GradientSpin {
    // GSPIN_ROW_TINTS: row0 cool blue, row1 amber, row2 pink.
    static let rowTints: [Color] = [
        Color(red: 0xB6 / 255, green: 0xD3 / 255, blue: 0xEF / 255),
        Color(red: 0xED / 255, green: 0xB1 / 255, blue: 0x85 / 255),
        Color(red: 0xF8 / 255, green: 0x88 / 255, blue: 0xA0 / 255),
    ]
    static let dim = 0.1

    /// Opacity keyframe (motion.rs gspin_opacity): full at 0, ease down to dim
    /// by 45%, hold to 92%, rise to full by 100%.
    static func opacity(phase: Double) -> Double {
        let p = phase.truncatingRemainder(dividingBy: 1)
        if p < 0.45 {
            let t = p / 0.45
            return 1 - (1 - dim) * (t * t * (3 - 2 * t))
        }
        if p < 0.92 { return dim }
        let t = (p - 0.92) / 0.08
        return dim + (1 - dim) * t
    }
}

/// 3×3 working indicator for the status strip (cell 2.5, arrow-up wave).
struct WorkingSpinner: View {
    var cellSize: CGFloat = 2.5
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate / Motion.gradientSpinPeriod
            grid(time: t)
        }
    }

    private func grid(time: Double) -> some View {
        VStack(spacing: cellSize * 0.8) {
            ForEach(0..<3, id: \.self) { row in
                HStack(spacing: cellSize * 0.8) {
                    ForEach(0..<3, id: \.self) { col in
                        let dx = Double(col - 1)
                        let dy = Double(2 - row)  // distance from bottom-center
                        let dist = (dx * dx + dy * dy).squareRoot() / 2.5
                        Rectangle()
                            .fill(GradientSpin.rowTints[row])
                            .frame(width: cellSize, height: cellSize)
                            .opacity(GradientSpin.opacity(phase: time - dist))
                    }
                }
            }
        }
    }
}

/// 2×3 mini spinner — cells snake clockwise around the perimeter ring
/// (loaders.rs mini_gradient_spinner). Used in session rows / tabs.
struct MiniSpinner: View {
    var cellSize: CGFloat = 2.0
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    // Perimeter order for a 2-wide × 3-tall grid, clockwise.
    private static let ring: [(row: Int, col: Int)] = [
        (0, 0), (0, 1), (1, 1), (2, 1), (2, 0), (1, 0),
    ]

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate / Motion.gradientSpinPeriod
            grid(time: t)
        }
    }

    private func grid(time: Double) -> some View {
        VStack(spacing: cellSize * 0.8) {
            ForEach(0..<3, id: \.self) { row in
                HStack(spacing: cellSize * 0.8) {
                    ForEach(0..<2, id: \.self) { col in
                        let ix = Self.ring.firstIndex { $0 == (row, col) } ?? 0
                        let phase = Double(ix) / Double(Self.ring.count)
                        Rectangle()
                            .fill(GradientSpin.rowTints[row])
                            .frame(width: cellSize, height: cellSize)
                            .opacity(GradientSpin.opacity(phase: time - phase))
                    }
                }
            }
        }
    }
}

/// zeron-pulse loading row: 5 cells, cosine wave, stagger 0.15/2.4
/// (loaders.rs:91).
struct ZeronPulse: View {
    var cellSize: CGFloat = 6
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate
            HStack(spacing: cellSize / 2) {
                ForEach(0..<5, id: \.self) { ix in
                    let phase = (t / Motion.zeronPulsePeriod - Double(ix) * (0.15 / 2.4))
                        .truncatingRemainder(dividingBy: 1)
                    let wave = (1 - cos(phase * 2 * .pi)) / 2
                    RoundedRectangle(cornerRadius: cellSize * 0.25)
                        .fill(Theme.text)
                        .frame(width: cellSize, height: cellSize)
                        .opacity(0.08 + 0.92 * wave)
                        .scaleEffect(0.9 + 0.1 * wave)
                }
            }
        }
    }
}

// MARK: - Transcript skeleton

/// Loading/settling placeholder in the transcript's own geometry — paragraph
/// clusters, a trailing user bubble, a tool chip — bottom-weighted like a real
/// conversation tail. Blocks breathe with a slight stagger; static under
/// reduced motion.
struct TranscriptSkeleton: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        GeometryReader { geo in
            let w = min(geo.size.width - 32, TranscriptView.maxContentWidth)
            TimelineView(.animation(paused: reduceMotion)) { timeline in
                let t = timeline.date.timeIntervalSinceReferenceDate
                VStack(alignment: .leading, spacing: 26) {
                    Spacer(minLength: 0)
                    paragraph(w, fractions: [0.92, 0.8, 0.55]).opacity(breathe(t, 0))
                    bubble(min(w * 0.6, 230)).opacity(breathe(t, 1))
                    paragraph(w, fractions: [0.85, 0.62]).opacity(breathe(t, 2))
                    chip(w * 0.7).opacity(breathe(t, 3))
                    paragraph(w, fractions: [0.9, 0.78, 0.42]).opacity(breathe(t, 4))
                }
                .padding(.horizontal, 16)
                // Clears the status strip + composer fade, like the real rows.
                .padding(.bottom, 56)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// 0.45→1 sine, 2s period, trailing blocks lag — reads as a soft wave
    /// rolling up the ghost conversation.
    private func breathe(_ t: TimeInterval, _ ix: Int) -> Double {
        guard !reduceMotion else { return 0.7 }
        let phase = t / 2 - Double(ix) * 0.12
        return 0.45 + 0.275 * (1 + sin(phase * 2 * .pi))
    }

    private func paragraph(_ width: CGFloat, fractions: [CGFloat]) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(fractions.enumerated()), id: \.offset) { _, frac in
                RoundedRectangle(cornerRadius: 4)
                    .fill(whiteAlpha(0.06))
                    .frame(width: width * frac, height: 12)
            }
        }
    }

    private func bubble(_ width: CGFloat) -> some View {
        RoundedRectangle(cornerRadius: Theme.bubbleRadius, style: .continuous)
            .fill(whiteAlpha(0.07))
            .frame(width: width, height: 42)
            .frame(maxWidth: .infinity, alignment: .trailing)
    }

    private func chip(_ width: CGFloat) -> some View {
        HStack(spacing: 8) {
            RoundedRectangle(cornerRadius: 5)
                .fill(whiteAlpha(0.07))
                .frame(width: 18, height: 18)
            RoundedRectangle(cornerRadius: 4)
                .fill(whiteAlpha(0.05))
                .frame(height: 10)
        }
        .padding(.horizontal, 8)
        .frame(width: width, height: 30)
        .background(whiteAlpha(0.03), in: RoundedRectangle(cornerRadius: 9))
    }
}

// MARK: - Status dot

extension ChatIndicator {
    /// shell/spaces.rs status_dot_color — non-done states are muted (running
    /// is routine); only Done keeps its pop.
    var dotColor: Color {
        switch self {
        case .working: return Theme.statusWorking.opacity(0.55)     // pink-400
        case .awaitingInput: return Theme.accent.opacity(0.6)       // indigo
        case .errored: return Theme.danger.opacity(0.65)
        case .completed: return Theme.statusCompleted.opacity(0.9)  // emerald-400
        case .idle: return whiteAlpha(0.14)
        }
    }

    /// shell.rs status word; nil (Idle) renders the time-ago instead.
    var label: String? {
        switch self {
        case .working: return "Working"
        case .awaitingInput: return "Input"
        case .errored: return "Failed"
        case .completed: return "Done"
        case .idle: return nil
        }
    }
}

/// The session row's top-right status glyph (shell.rs `render_chat_row`
/// corner slot): a 6pt dot with the status word beside it in the same color;
/// Done trades the dot for a check. Idle rows render time-ago instead — the
/// caller handles that branch, since only it knows the timestamp.
struct StatusCorner: View {
    let indicator: ChatIndicator

    var body: some View {
        HStack(spacing: 4) {
            if indicator == .completed {
                Image(systemName: "checkmark")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(indicator.dotColor)
            } else {
                Circle()
                    .fill(indicator.dotColor)
                    .frame(width: 6, height: 6)
            }
            if let label = indicator.label {
                Text(label)
                    .font(Theme.sans(10, weight: .medium))
                    .foregroundStyle(indicator.dotColor)
            }
        }
    }
}

/// Harness brand mark (pickers.rs harness_brand_icon) — the desktop's actual
/// SVG marks, rendered via BrandMarkShape. Claude keeps its brand orange even
/// on the mono surface; others stay neutral (icons.rs convention).
struct HarnessBadge: View {
    let harness: String
    var size: CGFloat = 14
    var dimmed = false
    /// Color for marks that carry no brand color of their own (codex, cursor).
    /// Claude keeps its orange regardless.
    var neutral: Color = Theme.text

    var body: some View {
        let mark = BrandMark.forHarness(harness)
        BrandMarkShape(mark: mark)
            .fill((BrandMark.brandTint(for: harness) ?? neutral).opacity(dimmed ? 0.6 : 0.9),
                  style: FillStyle(eoFill: mark.evenOddFill))
            .frame(width: size, height: size)
    }
}
