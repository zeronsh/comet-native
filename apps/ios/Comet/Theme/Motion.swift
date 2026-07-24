// Animation kit — timings/curves ported from crates/ui/src/motion.rs.
// Reduced-motion is honored at call sites via `motionAnimation(_:)`.

import SwiftUI

enum Motion {
    // Signature entrance: 500ms cubic-bezier(0.16, 1, 0.3, 1), translateY 4→0.
    static let fadeIn = Animation.timingCurve(0.16, 1, 0.3, 1, duration: 0.5)
    static let fadeQuick = Animation.timingCurve(0.25, 0.1, 0.25, 1, duration: 0.15)
    static let menuIn = Animation.timingCurve(0.25, 0.1, 0.25, 1, duration: 0.14)
    static let dialogIn = Animation.timingCurve(0.25, 0.1, 0.25, 1, duration: 0.18)
    static let resize = Animation.timingCurve(0, 0, 0.58, 1, duration: 0.2)
    static let collapse = Animation.timingCurve(0, 0, 0.58, 1, duration: 0.18)
    static let resort = Animation.timingCurve(0.22, 1, 0.36, 1, duration: 0.26)
    static let hoverFade = Animation.timingCurve(0.4, 0, 0.2, 1, duration: 0.15)

    // WorkingIndicator wave period (GRADIENT_SPIN) and loader pulse.
    static let gradientSpinPeriod: Double = 0.75
    static let cometPulsePeriod: Double = 2.4

    /// WorkingIndicator flavour words (transcript.rs:795), rotated every 7s,
    /// seeded per chat.
    static let flavourWords = [
        "Thinking", "Pondering", "Scheming", "Brewing", "Weaving", "Tinkering",
        "Musing", "Composing", "Sifting", "Untangling", "Distilling", "Sketching",
        "Plotting", "Riffing", "Combobulating", "Percolating", "Marinating",
        "Noodling", "Puzzling", "Conjuring",
    ]
    static let flavourRotateSecs: Int64 = 7

    static func flavourSeed(_ chatId: String) -> UInt64 {
        // FNV-1a, matching the desktop's per-chat seeding.
        var hash: UInt64 = 0xcbf29ce484222325
        for byte in chatId.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x100000001b3
        }
        return hash
    }

    static func flavourWord(seed: UInt64, elapsedSecs: Int64) -> String {
        let ix = Int((seed &+ UInt64(max(0, elapsedSecs) / flavourRotateSecs)) % UInt64(flavourWords.count))
        return flavourWords[ix]
    }

    static func formatElapsed(_ secs: Int64) -> String {
        let s = max(0, secs)
        if s < 60 { return "\(s)s" }
        return "\(s / 60)m \(s % 60)s"
    }
}

extension View {
    /// Apply an animation only when the user hasn't asked for reduced motion.
    func motionAnimation<V: Equatable>(_ animation: Animation, value: V) -> some View {
        modifier(MotionAnimationModifier(animation: animation, value: value))
    }
}

private struct MotionAnimationModifier<V: Equatable>: ViewModifier {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let animation: Animation
    let value: V

    func body(content: Content) -> some View {
        content.animation(reduceMotion ? nil : animation, value: value)
    }
}
