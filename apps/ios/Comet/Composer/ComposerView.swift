// Composer — the floating glass shell, a port of the old mobile app's
// composer (compact↔expanded morph, 36pt controls, focus-widen) carrying the
// desktop's Send→Steer→Stop semantics: live run + text = steer (same
// up-arrow), live run + empty = stop.
//
// The compact→expanded flip is deterministic (newline or >26 chars), NOT
// content-size measured — measurement oscillates at the boundary.

import SwiftUI

/// Shared glass shell + input + action row. `chips` (leading accessory views)
/// force the expanded layout — the desktop keeps new-session composers
/// expanded because the pickers need the full row.
struct ComposerShell<Chips: View>: View {
    @Binding var draft: String
    var placeholder = "Message"
    var sendEnabled: Bool
    var showStop: Bool
    var busy = false
    var onSend: () -> Void
    var onStop: () -> Void = {}
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool

    private var expanded: Bool {
        Chips.self != EmptyView.self || draft.contains("\n") || draft.count > 26
    }

    var body: some View {
        Group {
            if expanded {
                VStack(alignment: .leading, spacing: 0) {
                    input
                        .padding(.horizontal, 20)
                        .padding(.top, 15)
                    HStack(spacing: 10) {
                        // Chips scroll; the send button stays pinned.
                        ScrollView(.horizontal, showsIndicators: false) {
                            HStack(spacing: 8) {
                                chips
                            }
                        }
                        .scrollClipDisabled(false)
                        actionButton
                    }
                    .padding(.horizontal, 10)
                    .padding(.top, 10)
                    .padding(.bottom, 10)
                }
            } else {
                HStack(alignment: .center, spacing: 12) {
                    input
                        .padding(.leading, 20)
                        .padding(.vertical, 15)
                    actionButton
                        .padding(.trailing, 7)
                }
            }
        }
        .background(whiteAlpha(0.04), in: RoundedRectangle(cornerRadius: 28))
        .glassEffect(.regular.interactive(), in: RoundedRectangle(cornerRadius: 28))
        .overlay(RoundedRectangle(cornerRadius: 28).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        // Focus-widen: margins pull in slightly while typing (chat-session.tsx).
        .padding(.horizontal, focused ? 10 : 16)
        .motionAnimation(Motion.resize, value: focused)
        .motionAnimation(Motion.collapse, value: expanded)
    }

    private var input: some View {
        TextField(placeholder, text: $draft, axis: .vertical)
            .font(Theme.sans(16))
            .foregroundStyle(Theme.text)
            .tint(Theme.text)
            .lineLimit(1...7)
            .focused($focused)
    }

    private var actionButton: some View {
        Button {
            if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                onStop()
            } else {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onSend()
            }
        } label: {
            Group {
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.bg)
                } else if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    RoundedRectangle(cornerRadius: 3.5)
                        .fill(Theme.bg)
                        .frame(width: 12, height: 12)
                } else {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(buttonActive ? Theme.bg : Theme.textFaint)
                }
            }
            .frame(width: 36, height: 36)
            .background(buttonActive ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.10)),
                        in: Circle())
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(!buttonActive)
        .motionAnimation(Motion.fadeQuick, value: showStop)
    }

    private var buttonActive: Bool {
        if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return true }
        return sendEnabled && !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !busy
    }
}

/// The live-chat composer: config is locked once the chat exists, so no chips —
/// just the input and the morphing action button.
struct ComposerView: View {
    let store: SessionStore
    let chat: Chat
    let runLive: Bool

    @State private var text = ""

    var body: some View {
        ComposerShell(
            draft: $text,
            sendEnabled: true,
            showStop: runLive,
            onSend: send,
            onStop: { store.sendInterrupt() }
        ) {
            EmptyView()
        }
    }

    private func send() {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty else { return }
        if runLive {
            store.sendSteer(prompt: prompt)
        } else {
            store.sendRun(prompt: prompt, chat: chat)
        }
        text = ""
    }
}

// MARK: - Question panel (composer.rs Wizard)

struct QuestionPanel: View {
    let requestId: String
    let questions: [UserInputQuestion]
    let respond: (String, [UserInputAnswer]) -> Void

    @State private var page = 0
    @State private var picked: [String: Set<String>] = [:]  // questionId → labels
    @State private var typed: [String: String] = [:]
    @State private var autoAdvanceTask: Task<Void, Never>?

    var body: some View {
        let question = questions[min(page, questions.count - 1)]
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(question.header.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                Spacer()
                if questions.count > 1 {
                    Text("\(page + 1)/\(questions.count)")
                        .font(Theme.sans(10))
                        .foregroundStyle(Theme.textMuted)
                        .padding(.horizontal, 6)
                        .frame(height: 20)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }

            Text(question.question)
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if question.multiSelect == true {
                Text("Select one or more options.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
            }

            VStack(spacing: 4) {
                ForEach(Array(question.options.enumerated()), id: \.offset) { ix, option in
                    optionRow(question: question, ix: ix, option: option)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Rectangle().fill(whiteAlpha(0.06)).frame(height: 1)
                TextField("Or type your own answer", text: Binding(
                    get: { typed[question.id] ?? "" },
                    set: { typed[question.id] = $0 }
                ))
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .padding(.top, 6)
            }

            HStack {
                if page > 0 {
                    Button("Back") {
                        page -= 1
                    }
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(page < questions.count - 1 ? "Next" : "Submit") {
                    advance()
                }
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.bg)
                .padding(.horizontal, 16)
                .frame(height: 34)
                .background(Theme.text, in: Capsule())
                .opacity(canAdvance(question) ? 1 : 0.4)
                .disabled(!canAdvance(question))
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 26))
        .overlay(RoundedRectangle(cornerRadius: 26).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        .padding(.horizontal, 12)
        .transition(.opacity)
    }

    private func optionRow(question: UserInputQuestion, ix: Int, option: UserInputOption) -> some View {
        let isPicked = (typed[question.id] ?? "").isEmpty
            && picked[question.id, default: []].contains(option.label)
        return Button {
            pick(question: question, option: option)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(option.label)
                        .font(Theme.sans(13.5, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .multilineTextAlignment(.leading)
                    if let description = option.description, !description.isEmpty {
                        Text(description)
                            .font(Theme.sans(12))
                            .foregroundStyle(Theme.textMuted)
                            .multilineTextAlignment(.leading)
                    }
                }
                Spacer(minLength: 0)
                if ix < 9 {
                    Text("\(ix + 1)")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22, height: 22)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(isPicked ? whiteAlpha(0.09) : whiteAlpha(0.025),
                        in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12)
                .strokeBorder(isPicked ? whiteAlpha(0.16) : .clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func pick(question: UserInputQuestion, option: UserInputOption) {
        typed[question.id] = nil
        if question.multiSelect == true {
            var set = picked[question.id, default: []]
            if set.contains(option.label) { set.remove(option.label) } else { set.insert(option.label) }
            picked[question.id] = set
        } else {
            picked[question.id] = [option.label]
            // Single-select auto-advances after 220ms (AUTO_ADVANCE_MS).
            autoAdvanceTask?.cancel()
            autoAdvanceTask = Task {
                try? await Task.sleep(nanoseconds: 220_000_000)
                guard !Task.isCancelled else { return }
                advance()
            }
        }
    }

    private func canAdvance(_ question: UserInputQuestion) -> Bool {
        !(typed[question.id] ?? "").isEmpty || !picked[question.id, default: []].isEmpty
    }

    private func advance() {
        let question = questions[min(page, questions.count - 1)]
        guard canAdvance(question) else { return }
        if page < questions.count - 1 {
            page += 1
            return
        }
        let answers = questions.map { q -> UserInputAnswer in
            let typedAnswer = (typed[q.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !typedAnswer.isEmpty {
                return UserInputAnswer(questionId: q.id, labels: [typedAnswer])
            }
            return UserInputAnswer(questionId: q.id, labels: Array(picked[q.id, default: []]))
        }
        respond(requestId, answers)
    }
}
