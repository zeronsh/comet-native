import SwiftUI
import UIKit

/// A stable native editor keeps marked text, selection, and keyboard ownership
/// in one place. Sending commits and clears this same text storage synchronously.
@MainActor
final class ComposerEditorController: NSObject, UITextViewDelegate {
    weak var view: UITextView?
    var textChanged: (String) -> Void = { _ in }
    var focusChanged: (Bool) -> Void = { _ in }
    private var applying = false

    func commit() {
        guard let view else { return }
        applying = true
        view.unmarkText()
        applying = false
        textChanged(view.text)
    }

    func apply(text: String) {
        guard let view, view.text != text else { return }
        applying = true
        view.unmarkText()
        view.text = text
        view.selectedRange = NSRange(location: text.utf16.count, length: 0)
        if text.isEmpty { view.undoManager?.removeAllActions() }
        view.invalidateIntrinsicContentSize()
        applying = false
    }

    func textViewDidChange(_ textView: UITextView) {
        guard !applying else { return }
        textChanged(textView.text)
        textView.invalidateIntrinsicContentSize()
    }

    func textViewDidBeginEditing(_ textView: UITextView) { focusChanged(true) }
    func textViewDidEndEditing(_ textView: UITextView) { focusChanged(false) }
}

final class ComposerTextView: UITextView {
    var modifiedSubmit: () -> Void = {}
    override var keyCommands: [UIKeyCommand]? {
        let command = UIKeyCommand(input: "\r", modifierFlags: .command,
                                   action: #selector(submitFromKeyboard))
        command.discoverabilityTitle = "Send message or advance queue"
        command.wantsPriorityOverSystemBehavior = true
        return (super.keyCommands ?? []) + [command]
    }
    @objc func submitFromKeyboard() { modifiedSubmit() }
}

struct ComposerEditor: UIViewRepresentable {
    @Binding var text: String
    @Binding var focused: Bool
    let placeholder: String
    let maxLines: Int
    let controller: ComposerEditorController
    var onModifiedSubmit: () -> Void = {}
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    func makeUIView(context: Context) -> UITextView {
        let view = ComposerTextView()
        view.backgroundColor = .clear
        view.textContainerInset = .zero
        view.textContainer.lineFragmentPadding = 0
        view.showsVerticalScrollIndicator = false
        view.keyboardAppearance = .dark
        view.accessibilityIdentifier = "composer-input"
        view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        view.delegate = controller
        controller.view = view
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        (view as? ComposerTextView)?.modifiedSubmit = onModifiedSubmit
        controller.textChanged = { if text != $0 { text = $0 } }
        controller.focusChanged = { if focused != $0 { focused = $0 } }
        view.font = UIFontMetrics(forTextStyle: .body).scaledFont(
            for: UIFont(name: "Geist-Regular", size: 17) ?? .systemFont(ofSize: 17),
            compatibleWith: UITraitCollection(preferredContentSizeCategory: dynamicTypeSize.uiCategory))
        view.textColor = UIColor(Theme.text)
        view.tintColor = UIColor(Theme.text)
        view.accessibilityLabel = placeholder
        controller.apply(text: text)
        if focused, !view.isFirstResponder { view.becomeFirstResponder() }
        else if !focused, view.isFirstResponder { view.resignFirstResponder() }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: UITextView, context: Context) -> CGSize? {
        guard let width = proposal.width, width > 0 else { return nil }
        let line = uiView.font?.lineHeight ?? 22
        let height = uiView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height
        return CGSize(width: width, height: ceil(min(max(line, height), line * CGFloat(maxLines))))
    }
}

private extension DynamicTypeSize {
    var uiCategory: UIContentSizeCategory {
        switch self {
        case .xSmall: return .extraSmall
        case .small: return .small
        case .medium: return .medium
        case .large: return .large
        case .xLarge: return .extraLarge
        case .xxLarge: return .extraExtraLarge
        case .xxxLarge: return .extraExtraExtraLarge
        case .accessibility1: return .accessibilityMedium
        case .accessibility2: return .accessibilityLarge
        case .accessibility3: return .accessibilityExtraLarge
        case .accessibility4: return .accessibilityExtraExtraLarge
        case .accessibility5: return .accessibilityExtraExtraExtraLarge
        @unknown default: return .large
        }
    }
}
