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

/// comet-pulse loading row: 5 cells, cosine wave, stagger 0.15/2.4
/// (loaders.rs:91).
struct CometPulse: View {
    var cellSize: CGFloat = 6
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate
            HStack(spacing: cellSize / 2) {
                ForEach(0..<5, id: \.self) { ix in
                    let phase = (t / Motion.cometPulsePeriod - Double(ix) * (0.15 / 2.4))
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

// MARK: - Status dot

extension ChatIndicator {
    /// shell/spaces.rs status_dot_color.
    var dotColor: Color {
        switch self {
        case .working: return Theme.statusWorking.opacity(0.85)     // pink-400
        case .awaitingInput: return Theme.accent.opacity(0.9)       // indigo
        case .errored: return Theme.danger
        case .completed: return Theme.statusCompleted.opacity(0.9)  // emerald-400
        case .idle: return whiteAlpha(0.14)
        }
    }
}

/// The 6pt leading dot (leads so its position is stable); Working swaps in the
/// mini spinner.
struct StatusRail: View {
    let indicator: ChatIndicator

    var body: some View {
        Group {
            if indicator == .working {
                MiniSpinner()
            } else {
                Circle()
                    .fill(indicator.dotColor)
                    .frame(width: 6, height: 6)
            }
        }
        .frame(width: 8, height: 10)
    }
}

/// Harness brand mark (pickers.rs harness_brand_icon) — the desktop's actual
/// SVG marks, rendered via BrandMarkShape. Claude keeps its brand orange even
/// on the mono surface; others stay neutral (icons.rs convention).
struct HarnessBadge: View {
    let harness: String
    var size: CGFloat = 14
    var dimmed = false

    var body: some View {
        BrandMarkShape(mark: BrandMark.forHarness(harness))
            .fill(BrandMark.tint(for: harness).opacity(dimmed ? 0.6 : 0.9))
            .frame(width: size, height: size)
    }
}
