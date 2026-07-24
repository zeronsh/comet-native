// Session screen — transcript + status strip + composer (or question panel
// while input is requested, replacing the composer like the desktop). Reading
// marks the chat seen (the synced LWW marker behind the green dot everywhere).

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model
    let chatId: String
    @State private var showConfig = false
    @State private var refs: [RepoRef] = []
    @State private var catalogs: [String: [ModelInfo]] = [:]

    private var chat: Chat? { model.chat(id: chatId) }

    private var chatSpace: Space? {
        guard let spaceId = chat?.spaceId else { return nil }
        return model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        Group {
            if let chat, let store = model.sessionStore(for: chat) {
                content(chat: chat, store: store)
            } else {
                VStack(spacing: 12) {
                    CometPulse()
                    Text("Opening session…")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.bg)
            }
        }
        .navigationTitle(chat?.displayTitle ?? "Session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(.hidden, for: .navigationBar)
        .toolbar {
            if let chat {
                ToolbarItem(placement: .principal) {
                    // Tapping the header reconfigures model/effort mid-chat
                    // (the old app's header model pill); harness stays locked.
                    Button {
                        showConfig = true
                    } label: {
                        VStack(spacing: 1) {
                            HStack(spacing: 6) {
                                HarnessBadge(harness: chat.config?.harness ?? "claude-code", size: 12)
                                Text(chat.displayTitle)
                                    .font(Theme.sans(13, weight: .medium))
                                    .foregroundStyle(Theme.text)
                                    .lineLimit(1)
                                Image(systemName: "chevron.down")
                                    .font(.system(size: 8, weight: .semibold))
                                    .foregroundStyle(Theme.textFaint)
                            }
                            if let subtitle {
                                Text(subtitle)
                                    .font(Theme.sans(10.5))
                                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                                    .lineLimit(1)
                            }
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .sheet(isPresented: $showConfig) {
            if let chat {
                let harness = chat.config?.harness ?? "claude-code"
                ModelPickerSheet(
                    harness: .constant(harness),
                    modelId: Binding(
                        get: {
                            chat.config?.model
                                ?? HarnessCatalog.defaultModel(for: harness).id
                        },
                        set: { newModel in
                            writeConfig(model: newModel, reasoning: chat.config?.reasoning)
                        }
                    ),
                    reasoning: Binding(
                        get: { chat.config?.reasoning },
                        set: { newReasoning in
                            writeConfig(model: chat.config?.model, reasoning: newReasoning)
                        }
                    ),
                    lockedHarness: true,
                    catalogs: catalogs,
                    checkout: checkoutContext(chat: chat)
                )
            }
        }
        .task(id: chatId) {
            guard let space = chatSpace else { return }
            let harness = chat?.config?.harness ?? "claude-code"
            catalogs[harness] = await model.listModels(space: space, harness: harness)
            guard space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                refs = loaded
            }
        }
        .onAppear {
            model.markSeen(chatId: chatId)
            if model.launchSheet == "config" {
                model.launchSheet = nil
                showConfig = true
            }
        }
        .onDisappear {
            model.markSeen(chatId: chatId)
            model.releaseSessionStore(chatId: chatId)
        }
    }

    /// Live-chat checkout context (git spaces only): read-only kind + the
    /// switchable ref list.
    private func checkoutContext(chat: Chat) -> SessionCheckoutContext? {
        guard let space = chatSpace, space.gitDetected, let cwd = chat.cwd else { return nil }
        return SessionCheckoutContext(
            isWorktree: cwd != space.path,
            cwd: cwd,
            refs: refs,
            currentBranch: chat.branch,
            onPick: { ref in
                let error = await model.switchSessionRef(chat: chat, ref: ref)
                if error == nil, let reloaded = await model.listRefs(space: space) {
                    refs = reloaded
                }
                return error
            }
        )
    }

    /// Merge a model/effort change into the chat's config row (LWW; the host
    /// picks it up on the next run dispatch).
    private func writeConfig(model newModel: String?, reasoning newReasoning: String?) {
        guard let chat else { return }
        var config = chat.config ?? ChatConfig(harness: "claude-code", model: nil,
                                               reasoning: nil, sandbox: "workspace-write")
        config.model = newModel
        config.reasoning = newReasoning
        model.setChatConfig(chatId: chat.id, config: config)
    }

    private var subtitle: String? {
        guard let chat else { return nil }
        var parts: [String] = []
        if let cwd = chat.cwd { parts.append((cwd as NSString).lastPathComponent) }
        if let branch = chat.branch, !branch.isEmpty { parts.append(branch) }
        parts.append(model.deviceName(chat.deviceId))
        return parts.joined(separator: " · ")
    }

    private func content(chat: Chat, store: SessionStore) -> some View {
        let status = liveStatus(chat: chat)
        return VStack(spacing: 0) {
            TranscriptView(store: store, chatId: chat.id)

            statusStrip(chat: chat, status: status)

            if let request = store.openInputRequest {
                QuestionPanel(requestId: request.requestId, questions: request.questions) { requestId, answers in
                    store.respondInput(requestId: requestId, answers: answers)
                }
                .padding(.bottom, 8)
            } else {
                ComposerView(store: store, chat: chat, runLive: status == .working)
                    .padding(.bottom, 8)
            }
        }
        .background(Theme.bg.ignoresSafeArea())
        .motionAnimation(Motion.fadeQuick, value: store.openInputRequest?.requestId)
    }

    private func liveStatus(chat: Chat) -> SessionStatus? {
        if let demo = model.demo {
            return effectiveStatus(demo.sessions[chat.id], now: nowMs())
        }
        return effectiveStatus(model.workspace?.sessions[chat.id], now: nowMs())
    }

    /// Reserved 24pt status strip (shell.rs render_status_strip) — Working
    /// shows the sunrise spinner + rotating flavour word + elapsed; Errored
    /// shows "Run failed"; the strip always reserves its height so the
    /// composer never shifts.
    private func statusStrip(chat: Chat, status: SessionStatus?) -> some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            HStack(spacing: 6) {
                switch status {
                case .working:
                    WorkingSpinner()
                    let startedAt = sessionStartedAt(chat: chat)
                    let elapsed = (nowMs() - startedAt) / 1000
                    Text("\(Motion.flavourWord(seed: Motion.flavourSeed(chat.id), elapsedSecs: elapsed))…")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                    Text(Motion.formatElapsed(elapsed))
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textFaint)
                        .monospacedDigit()
                case .errored:
                    Text("Run failed")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.danger)
                default:
                    EmptyView()
                }
            }
            .frame(height: 24)
            .frame(maxWidth: .infinity)
        }
    }

    private func sessionStartedAt(chat: Chat) -> Int64 {
        let row = model.demo?.sessions[chat.id] ?? model.workspace?.sessions[chat.id]
        return row?.startedAt ?? row?.updatedAt ?? nowMs()
    }
}
