// Session screen — transcript + status strip + composer (or question panel
// while input is requested, replacing the composer like the desktop). Reading
// marks the chat seen (the synced LWW marker behind the green dot everywhere).

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    let chatId: String

    /// Width the nav bar's own controls need around a LEADING title — the
    /// back button ahead of it, bar margins, and slack. Generous on purpose:
    /// a fixed-width item that does NOT fit gets evicted into a trailing "…"
    /// overflow menu (where a custom text stack renders as nothing) — seen on
    /// iPhone Air at 110.
    private static let headerChromeInset: CGFloat = 170

    /// The view's own width, the only reliable basis for capping the principal
    /// toolbar item (its container proposes an unbounded width).
    @State private var viewWidth: CGFloat = 0

    /// Follow intent belongs to the session, independent of composer focus.
    @State private var scroll = ScrollState()


    private var chat: Chat? { model.chat(id: chatId) }

    private var chatSpace: Space? {
        guard let spaceId = chat?.spaceId else { return nil }
        return model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        Group {
            if let chat, let store = model.sessionStore(for: chat) {
                content(chat: chat, store: store)
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { viewWidth = $0 }
            } else {
                VStack(spacing: 12) {
                    ZeronPulse()
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
        .toolbar(removing: .title)  // the leading header owns the bar
        .toolbarBackground(.hidden, for: .navigationBar)
        .toolbar {
            if let chat {
                // Static, left-aligned session header — model/effort changes
                // moved into the composer's picker chips.
                ToolbarItem(placement: .topBarLeading) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(chat.displayTitle)
                            .font(Theme.sans(15, weight: .medium))
                            .foregroundStyle(Theme.text)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        if let subtitle {
                            Text(subtitle)
                                .font(Theme.sans(12))
                                .foregroundStyle(Theme.textMuted.opacity(0.6))
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    }
                    // A FIXED width, not a max: iOS 26 proposes leading items
                    // almost nothing next to the back button, so a flexible
                    // frame collapses to its minimum ("S…"). Claiming the
                    // remainder of the bar outright lays the texts out with
                    // real room and truncates them properly.
                    .frame(width: max(140, viewWidth - Self.headerChromeInset),
                           alignment: .leading)
                }
                // Bare text on the bar, not a glass capsule.
                .sharedBackgroundVisibility(.hidden)
            }
        }
        .onAppear {
            model.markSeen(chatId: chatId)
        }
        .onDisappear {
            model.markSeen(chatId: chatId)
            model.releaseSessionStore(chatId: chatId)
        }
    }

    /// "space @ device" — short, like the home dropdown's rows. The space
    /// NAME (not the cwd basename: they differ for renamed spaces and
    /// worktree sessions), falling back to the cwd when the space row is gone.
    private var subtitle: String? {
        guard let chat else { return nil }
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        return "\(space) @ \(model.deviceName(chat.deviceId))"
    }

    private func content(chat: Chat, store: SessionStore) -> some View {
        let status = liveStatus(chat: chat)
        // The composer owns real layout space. The transcript's viewport ends
        // above it, so keyboard and glass morphs cannot cover the last row.
        return VStack(spacing: 0) {
            TranscriptView(store: store, chatId: chat.id, scroll: scroll)
                .overlay {
                    if store.entries.isEmpty, store.pendingSends.isEmpty,
                       chat.lastMessageAt != nil {
                        TranscriptSkeleton().background(Theme.bg)
                    }
                }
                .motionAnimation(Motion.fadeQuick, value: store.entries.isEmpty)
            VStack(spacing: 0) {
                if verticalSizeClass != .compact || status == .working || status == .errored
                    || model.sendState(for: chat) != nil {
                    statusStrip(chat: chat, status: status, store: store)
                        .allowsHitTesting(model.sendState(for: chat) == .failed)
                }
                Group {
                    if let request = store.openInputRequest {
                        QuestionPanel(requestId: request.requestId, questions: request.questions) { requestId, answers in
                            store.respondInput(requestId: requestId, answers: answers)
                        }
                    } else {
                        ComposerView(store: store, chat: chat, runLive: status == .working)
                    }
                }
                .padding(.bottom, 8)
            }
            .background {
                LinearGradient(
                    stops: [
                        .init(color: Theme.bg.opacity(0), location: 0),
                        .init(color: Theme.bg.opacity(0.45), location: 0.25),
                        .init(color: Theme.bg.opacity(0.72), location: 0.6),
                        .init(color: Theme.bg, location: 1),
                    ],
                    startPoint: .top, endPoint: .bottom
                )
                .padding(.top, verticalSizeClass == .compact ? 0 : -24)
                .ignoresSafeArea(.container, edges: .bottom)
                .allowsHitTesting(false)
            }
        }
        .background(Theme.bg.ignoresSafeArea())
        .overlay {
            GeometryReader { geometry in
                let fadeHeight = min(64, geometry.safeAreaInsets.top)
                VStack(spacing: 0) {
                    Theme.bg.frame(height: geometry.safeAreaInsets.top - fadeHeight)
                    LinearGradient(stops: [
                        .init(color: Theme.bg, location: 0),
                        .init(color: Theme.bg.opacity(0.96), location: 0.5),
                        .init(color: Theme.bg.opacity(0.8), location: 0.75),
                        .init(color: Theme.bg.opacity(0), location: 1),
                    ], startPoint: .top, endPoint: .bottom)
                        .frame(height: fadeHeight)
                }
                .offset(y: -geometry.safeAreaInsets.top)
            }
            .allowsHitTesting(false)
            .accessibilityHidden(true)
        }
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
    /// composer never shifts. An unadopted send's truth takes precedence:
    /// "Sending…" (healthy, within the 2-minute grace), "Queued — will send
    /// automatically" (degraded path — no fake progress), or the explicit
    /// "Not delivered — tap to retry" (transcript.rs retry_send).
    private func statusStrip(chat: Chat, status: SessionStatus?, store: SessionStore) -> some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            HStack(spacing: 6) {
                switch model.sendState(for: chat) {
                case .failed?:
                    Button {
                        store.retryDelivery()
                    } label: {
                        Text("Not delivered — tap to retry")
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.danger)
                    }
                    .buttonStyle(.plain)
                case .queued?:
                    Circle()
                        .fill(Theme.warning)
                        .frame(width: 5, height: 5)
                    Text("Queued — will send automatically")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.warning.opacity(0.9))
                case .sending?:
                    // The percent tracks the REAL relay transfer (escort
                    // bytes committed to the host), not just local staging.
                    if let progress = store.transferProgress {
                        Text("Uploading… \(Int(progress * 100))%")
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.textMuted)
                            .monospacedDigit()
                    } else {
                        Text("Sending…")
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.textMuted)
                    }
                case nil:
                    normalStatus(chat: chat, status: status)
                }
            }
            .frame(height: 24)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.leading, 26)  // aligns with the composer's text start
        }
    }

    @ViewBuilder
    private func normalStatus(chat: Chat, status: SessionStatus?) -> some View {
        Group {
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
    }

    private func sessionStartedAt(chat: Chat) -> Int64 {
        let row = model.demo?.sessions[chat.id] ?? model.workspace?.sessions[chat.id]
        return row?.startedAt ?? row?.updatedAt ?? nowMs()
    }
}
