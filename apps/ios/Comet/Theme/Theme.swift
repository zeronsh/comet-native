// Always-dark monochrome theme — a direct port of crates/ui/src/theme.rs.
//
// Colors are computed from the same oklch definitions the desktop app uses
// (Björn Ottosson's OKLab matrices, the ones CSS Color 4 specifies), so every
// surface and accent lands on identical sRGB values. **Numbers drive layout,
// colors are paint**: layout constants are plain numbers and never depend on
// which color is painted.

import SwiftUI

enum Theme {
    // ---- paint: neutral surfaces (oklch chroma 0) ----
    /// Main panel background — sampled #060606.
    static let bg = grey(6)
    /// Shell / sidebar surface — sampled #0d0d0d.
    static let surface = grey(13)
    /// Raised surface: popovers, dialogs, cards.
    static let surfaceRaised = neutral(0.235)
    /// Hover/pressed wash for interactive rows (white, low alpha).
    static let elementHover = whiteAlpha(0.06)
    /// Active/selected wash.
    static let elementActive = whiteAlpha(0.10)
    /// Hairline border — white at low alpha so it reads on any surface.
    static let border = whiteAlpha(0.08)
    /// Stronger border for focused/raised edges.
    static let borderStrong = whiteAlpha(0.14)

    // ---- paint: text ----
    static let text = neutral(0.922)       // ~neutral-200
    static let textMuted = neutral(0.708)  // ~neutral-400
    static let textFaint = neutral(0.556)  // ~neutral-500

    // ---- paint: accents ----
    static let accent = oklch(0.673, 0.182, 276.935)        // indigo-400
    static let accentStrong = oklch(0.585, 0.233, 277.117)  // indigo-500
    static let danger = oklch(0.704, 0.191, 22.216)         // red-400
    static let dangerSoft = oklch(0.808, 0.114, 19.571)     // red-300
    static let warning = oklch(0.828, 0.189, 84.429)        // amber-400

    // ---- paint: status dots (shell/spaces.rs status_dot_color) ----
    static let statusWorking = oklch(0.718, 0.202, 349.761)   // pink-400
    static let statusCompleted = oklch(0.765, 0.177, 163.223) // emerald-400
    /// Claude brand orange — kept even on the mono surface.
    static let claudeBrand = Color(red: 0xD9 / 255.0, green: 0x77 / 255.0, blue: 0x57 / 255.0)

    // ---- paint: markdown inline code (violet family) ----
    static let inlineCodeText = oklch(0.811, 0.111, 293.571)  // violet-300
    static let inlineCodeWash = oklch(0.702, 0.183, 293.541).opacity(0.12) // violet-400 @ 0.12

    // ---- paint: syntax tokens (soft, paint-only) ----
    static let tokenKeyword = oklch(0.709, 0.129, 20.0)   // soft rose
    static let tokenString = oklch(0.770, 0.110, 168.0)   // soft green
    static let tokenNumber = oklch(0.780, 0.120, 80.0)    // soft amber

    // ---- numbers drive layout (pt) ----
    static let bubbleRadius: CGFloat = 16
    static let panelRadius: CGFloat = 10
    static let controlRadius: CGFloat = 6
    static let spaceXS: CGFloat = 4
    static let spaceSM: CGFloat = 8
    static let spaceMD: CGFloat = 12
    static let spaceLG: CGFloat = 16
}

// MARK: - Fonts

extension Theme {
    static let fontSansName = "Geist"
    static let fontMonoName = "GeistMono-Regular"

    static func sans(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        // Static weight cuts register as separate families — select by
        // PostScript name so weights actually resolve.
        let name: String
        if weight == .medium {
            name = "Geist-Medium"
        } else if weight == .semibold {
            name = "Geist-SemiBold"
        } else if weight == .bold {
            name = "Geist-Bold"
        } else {
            name = "Geist-Regular"
        }
        return .custom(name, size: size)
    }

    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .custom(fontMonoName, size: size).weight(weight)
    }

    static func sansUI(_ size: CGFloat, weight: UIFont.Weight = .regular) -> UIFont {
        let traits: [UIFontDescriptor.TraitKey: Any] = [.weight: weight]
        let descriptor = UIFontDescriptor(fontAttributes: [
            .family: "Geist",
            .traits: traits,
        ])
        return UIFont(descriptor: descriptor, size: size)
    }

    static func monoUI(_ size: CGFloat) -> UIFont {
        UIFont(name: fontMonoName, size: size)
            ?? .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

// MARK: - Color primitives (ported from theme.rs)

/// A neutral (chroma 0) oklch tone. Chroma 0 means r == g == b exactly.
func neutral(_ lightness: Double) -> Color {
    let v = Double(oklchToSrgb(l: lightness, c: 0, hDeg: 0)[0])
    return Color(red: v, green: v, blue: v)
}

/// White at the given alpha — the hairline/wash primitive.
func whiteAlpha(_ alpha: Double) -> Color {
    Color.white.opacity(alpha)
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ #0d0d0d).
func grey(_ value: UInt8) -> Color {
    let v = Double(value) / 255.0
    return Color(red: v, green: v, blue: v)
}

/// oklch (CSS notation: L 0..1, C, H degrees) → sRGB Color.
func oklch(_ l: Double, _ c: Double, _ hDeg: Double) -> Color {
    let rgb = oklchToSrgb(l: l, c: c, hDeg: hDeg)
    return Color(red: Double(rgb[0]), green: Double(rgb[1]), blue: Double(rgb[2]))
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
func oklchToSrgb(l: Double, c: Double, hDeg: Double) -> [Double] {
    let h = hDeg * .pi / 180
    let a = c * cos(h)
    let b = c * sin(h)

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.39633778 * a + 0.21580376 * b
    let m_ = l - 0.105561346 * a - 0.06385417 * b
    let s_ = l - 0.08948418 * a - 1.2914855 * b
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_)

    // LMS → linear sRGB
    let r = 4.0767417 * l3 - 3.3077116 * m3 + 0.23096993 * s3
    let g = -1.268438 * l3 + 2.6097574 * m3 - 0.3413194 * s3
    let bl = -0.0041960863 * l3 - 0.7034186 * m3 + 1.7076147 * s3

    return [gammaEncode(r), gammaEncode(g), gammaEncode(bl)]
}

private func gammaEncode(_ x: Double) -> Double {
    let x = min(max(x, 0), 1)
    return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055
}
