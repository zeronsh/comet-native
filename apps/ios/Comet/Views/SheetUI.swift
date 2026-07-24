// Sheet design language — the mobile app's grouped-card system (a port of the
// old app's sheet-ui.tsx: panel cards, hairline-separated rows, centered
// headers), restated in this app's monochrome theme. Every sheet composes
// these pieces so they all feel like one product.

import SwiftUI

enum SheetStyle {
    static let cardRadius: CGFloat = 20
    static let cardFill = whiteAlpha(0.045)
    static let rowSeparator = whiteAlpha(0.06)
    static let panel = grey(0x14)
}

/// Grouped card: rows separated by inset hairlines.
struct SheetCard<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(spacing: 0) {
            content
        }
        .background(SheetStyle.cardFill, in: RoundedRectangle(cornerRadius: SheetStyle.cardRadius))
        .overlay(RoundedRectangle(cornerRadius: SheetStyle.cardRadius)
            .strokeBorder(whiteAlpha(0.06), lineWidth: 1))
    }
}

/// Inset hairline between card rows.
struct SheetSeparator: View {
    var body: some View {
        Rectangle()
            .fill(SheetStyle.rowSeparator)
            .frame(height: 1)
            .padding(.leading, 16)
    }
}

/// Selectable row: title + optional subtitle, accent check when selected.
struct SheetSelectRow: View {
    let title: String
    var subtitle: String?
    var selected: Bool
    var leading: AnyView?
    let action: () -> Void

    var body: some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            action()
        } label: {
            HStack(spacing: 12) {
                if let leading {
                    leading
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                            .lineLimit(2)
                    }
                }
                Spacer(minLength: 8)
                Image(systemName: "checkmark")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.text)                    .opacity(selected ? 1 : 0)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}

/// Navigation-style row: title + trailing detail + chevron.
struct SheetLinkRow: View {
    let title: String
    var detail: String?
    var systemImage: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                if let systemImage {
                    Image(systemName: systemImage)
                        .font(.system(size: 15))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22)
                }
                Text(title)
                    .font(Theme.sans(15))
                    .foregroundStyle(Theme.text)
                Spacer(minLength: 8)
                if let detail {
                    Text(detail)
                        .font(Theme.sans(14))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                }
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}

/// Uppercase tracked section label above a card.
struct SheetLabel: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(text.uppercased())
            .font(Theme.sans(11, weight: .medium))
            .kerning(1)
            .foregroundStyle(Theme.textMuted.opacity(0.6))
            .padding(.horizontal, 4)
    }
}

/// Row press feedback: brief white wash, like UIKit cell highlight.
struct SheetRowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? whiteAlpha(0.06) : .clear)
    }
}

/// Primary pill button pinned at a sheet's bottom.
struct SheetPrimaryButton: View {
    let title: String
    var enabled = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(Theme.sans(15, weight: .semibold))
                .foregroundStyle(enabled ? Theme.bg : Theme.textFaint)
                .frame(maxWidth: .infinity)
                .frame(height: 50)
                .background(enabled ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.08)),
                            in: Capsule())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
    }
}

/// Pressed-state wash for tappable rows and chips — the desktop's
/// `element_hover` (white 6%) translated to touch. Fades out on release.
struct PressWashButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat = 8

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? Theme.elementHover : Color.clear,
                        in: RoundedRectangle(cornerRadius: cornerRadius))
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

/// Capsule variant for chips: deepens the existing fill while pressed.
struct ChipPressButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .overlay(Capsule().fill(configuration.isPressed ? whiteAlpha(0.06) : .clear))
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
