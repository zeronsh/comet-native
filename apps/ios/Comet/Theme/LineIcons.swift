// Stroked UI glyphs — the desktop's hand-drawn Solar-Linear-style icons
// (crates/ui/assets/icons), rendered natively: same path data, stroked at the
// same 1.5/24 weight with round caps, tinted by the foreground style.

import SwiftUI

enum LineIcon {
    case gitBranch
    case folder
    case folderWithFiles

    /// (paths, circles cx/cy/r) in a 24×24 viewbox.
    var elements: (paths: [String], circles: [(CGFloat, CGFloat, CGFloat)]) {
        switch self {
        case .gitBranch:
            return (
                paths: [
                    "M6.5 7.75v8.5",
                    "M17.5 9.75c0 2.9-2.6 4.35-6.2 4.72c-1.9.2-3.3.9-4 2.03",
                ],
                circles: [(6.5, 5.5, 2.25), (6.5, 18.5, 2.25), (17.5, 7.5, 2.25)]
            )
        case .folder:
            return (
                paths: [
                    "M18 10h-5",
                    "M2 6.95c0-.883 0-1.324.07-1.692A4 4 0 0 1 5.257 2.07C5.626 2 6.068 2 6.95 2c.386 0 .58 0 .766.017a4 4 0 0 1 2.18.904c.144.119.28.255.554.529L11 4c.816.816 1.224 1.224 1.712 1.495a4 4 0 0 0 .848.352C14.098 6 14.675 6 15.828 6h.374c2.632 0 3.949 0 4.804.77q.119.105.224.224c.77.855.77 2.172.77 4.804V14c0 3.771 0 5.657-1.172 6.828S17.771 22 14 22h-4c-3.771 0-5.657 0-6.828-1.172S2 17.771 2 14z",
                ],
                circles: []
            )
        case .folderWithFiles:
            return (
                paths: [
                    "M18 10h-5",
                    "M10 3h6.5c.464 0 .697 0 .892.026a3 3 0 0 1 2.582 2.582c.026.195.026.428.026.892",
                    "M2 6.95c0-.883 0-1.324.07-1.692A4 4 0 0 1 5.257 2.07C5.626 2 6.068 2 6.95 2c.386 0 .58 0 .766.017a4 4 0 0 1 2.18.904c.144.119.28.255.554.529L11 4c.816.816 1.224 1.224 1.712 1.495a4 4 0 0 0 .848.352C14.098 6 14.675 6 15.828 6h.374c2.632 0 3.949 0 4.804.77q.119.105.224.224c.77.855.77 2.172.77 4.804V14c0 3.771 0 5.657-1.172 6.828S17.771 22 14 22h-4c-3.771 0-5.657 0-6.828-1.172S2 17.771 2 14z",
                ],
                circles: []
            )
        }
    }
}

struct LineIconShape: Shape {
    let icon: LineIcon

    func path(in rect: CGRect) -> Path {
        var combined = Path()
        let elements = icon.elements
        for data in elements.paths {
            combined.addPath(SVGPathParser.path(from: data))
        }
        for (cx, cy, r) in elements.circles {
            combined.addEllipse(in: CGRect(x: cx - r, y: cy - r, width: r * 2, height: r * 2))
        }
        let scale = min(rect.width, rect.height) / 24
        let dx = rect.minX + (rect.width - 24 * scale) / 2
        let dy = rect.minY + (rect.height - 24 * scale) / 2
        return combined.applying(CGAffineTransform(scaleX: scale, y: scale)
            .concatenating(CGAffineTransform(translationX: dx, y: dy)))
    }
}

/// An icon element: `LineIconView(.gitBranch, size: 12, color: …)`.
struct LineIconView: View {
    let icon: LineIcon
    var size: CGFloat = 14
    var color: Color = Theme.textMuted

    init(_ icon: LineIcon, size: CGFloat = 14, color: Color = Theme.textMuted) {
        self.icon = icon
        self.size = size
        self.color = color
    }

    var body: some View {
        LineIconShape(icon: icon)
            .stroke(color, style: StrokeStyle(lineWidth: 1.5 * size / 24,
                                              lineCap: .round, lineJoin: .round))
            .frame(width: size, height: size)
    }
}
