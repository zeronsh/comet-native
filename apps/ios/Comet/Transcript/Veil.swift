// Streaming fade-in veil — a port of crates/ui/src/markdown/veil.rs.
//
// Paint-only: newly appended text dissolves in by multiplying a fading alpha
// into its color. Spans split at chunk boundaries and never change layout.
// Fade duration tracks the append cadence (EMA of inter-chunk gaps); a
// re-attach seeds the baseline so already-streamed text never re-fades.

import Foundation
import SwiftUI

final class RowVeil {
    // veil.rs constants.
    static let emaSeedMs: Double = 160
    static let minFadeMs: Double = 120
    static let maxFadeMs: Double = 400
    static let curvePow: Double = 1.6
    static let gapClampMs: Double = 1000

    private struct Span {
        var range: Range<Int>  // character offsets in the row's source text
        var startMs: Double
        var durationMs: Double
    }

    private var spans: [Span] = []
    private var settledLength: Int
    private var emaMs: Double = RowVeil.emaSeedMs
    private var lastAppendMs: Double?

    init(seededLength: Int = 0) {
        settledLength = seededLength
    }

    /// Register growth to `newLength`; the delta becomes a fading span.
    func noteLength(_ newLength: Int) {
        guard newLength > settledLength + spans.map(\.range.count).reduce(0, +) else { return }
        let now = Date().timeIntervalSince1970 * 1000
        if let last = lastAppendMs {
            let gap = min(now - last, Self.gapClampMs)
            emaMs = emaMs * 0.7 + gap * 0.3
        }
        lastAppendMs = now
        let covered = settledLength + spans.map(\.range.count).reduce(0, +)
        // Fast-stream boost: concurrent chunks fade slightly slower.
        let active = spans.filter { now - $0.startMs < $0.durationMs }.count
        let boost = 1 + 0.3 * Double(max(0, active - 2))
        let duration = min(max(emaMs * 3, Self.minFadeMs), Self.maxFadeMs) * boost
        spans.append(Span(range: covered..<newLength, startMs: now, durationMs: duration))
        prune(now: now)
    }

    private func prune(now: Double) {
        var absorbed = 0
        for span in spans {
            if now - span.startMs >= span.durationMs {
                absorbed = max(absorbed, span.range.upperBound)
            } else {
                break
            }
        }
        if absorbed > 0 {
            settledLength = max(settledLength, absorbed)
            spans.removeAll { $0.range.upperBound <= absorbed }
        }
    }

    var isFading: Bool {
        let now = Date().timeIntervalSince1970 * 1000
        return spans.contains { now - $0.startMs < $0.durationMs }
    }

    /// Alpha curve: 1 − (1−p)^1.6 — fast attack, soft landing.
    static func opacity(progress: Double) -> Double {
        let p = min(max(progress, 0), 1)
        return 1 - pow(1 - p, curvePow)
    }

    /// The veil as contiguous (range, alpha) segments over a text of
    /// `totalLength` characters — settled text alpha 1, fading spans partial.
    /// Drives the Text-concatenation render path (rounded code washes need
    /// per-segment Text pieces, not AttributedString mutation).
    func segments(totalLength: Int) -> [(range: Range<Int>, alpha: Double)] {
        let now = Date().timeIntervalSince1970 * 1000
        var out: [(Range<Int>, Double)] = []
        var cursor = 0
        for span in spans.sorted(by: { $0.range.lowerBound < $1.range.lowerBound }) {
            let lower = min(span.range.lowerBound, totalLength)
            let upper = min(span.range.upperBound, totalLength)
            if lower > cursor {
                out.append((cursor..<lower, 1))
            }
            if upper > lower {
                let progress = min(max((now - span.startMs) / span.durationMs, 0), 1)
                out.append((lower..<upper, Self.opacity(progress: progress)))
            }
            cursor = max(cursor, upper)
        }
        if cursor < totalLength {
            out.append((cursor..<totalLength, 1))
        }
        return out
    }

    /// Multiply fading alphas into an AttributedString whose characters map
    /// 1:1 onto the veiled source range starting at `sourceOffset`.
    func apply(to attr: inout AttributedString, sourceOffset: Int) {
        let now = Date().timeIntervalSince1970 * 1000
        let length = attr.characters.count
        for span in spans {
            let progress = min(max((now - span.startMs) / span.durationMs, 0), 1)
            if progress >= 1 { continue }
            let alpha = Self.opacity(progress: progress)
            let lo = max(0, span.range.lowerBound - sourceOffset)
            let hi = min(length, span.range.upperBound - sourceOffset)
            guard lo < hi,
                  let start = attr.characters.index(attr.startIndex, offsetBy: lo, limitedBy: attr.endIndex),
                  let end = attr.characters.index(attr.startIndex, offsetBy: hi, limitedBy: attr.endIndex)
            else { continue }
            for run in attr[start..<end].runs {
                let base: Color = attr[run.range].foregroundColor ?? Theme.text
                // `.opacity` multiplies into the existing alpha — paint only.
                attr[run.range].foregroundColor = base.opacity(alpha)
            }
        }
    }
}
