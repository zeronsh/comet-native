// Rounded inline-code washes — the desktop paints violet rounded quads behind
// code runs (render.rs INLINE_CODE_RADIUS 4.5, x-overhang 2, y-inset 2);
// AttributedString's backgroundColor can only draw square boxes, so runs are
// tagged with a custom attribute and a TextRenderer paints the pills beneath
// the glyphs. Paint-only: layout is untouched.

import SwiftUI

/// Marks a run as inline code for the renderer.
struct InlineCodeAttribute: TextAttribute {}

struct InlineCodeRenderer: TextRenderer {
    var wash: Color = Theme.inlineCodeWash

    func draw(layout: Text.Layout, in context: inout GraphicsContext) {
        // Wash pass: one rounded rect per contiguous tagged slice range.
        for line in layout {
            for run in line {
                guard run[InlineCodeAttribute.self] != nil else { continue }
                let bounds = run.typographicBounds.rect
                let rect = CGRect(x: bounds.minX - 2,
                                  y: bounds.minY + 2,
                                  width: bounds.width + 4,
                                  height: bounds.height - 4)
                context.fill(Path(roundedRect: rect, cornerRadius: 4.5), with: .color(wash))
            }
        }
        // Glyph pass.
        for line in layout {
            context.draw(line)
        }
    }
}
