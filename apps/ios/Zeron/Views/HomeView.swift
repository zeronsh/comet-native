// Home — the mobile shell. The desktop sidebar collapses into one screen: a
// space dropdown in the nav bar (default "All") scopes the attention-sorted
// session list below it. Tabs-as-sessions don't fit a phone; close=archive
// becomes swipe-to-archive.

import SwiftUI

enum Route: Hashable {
    case space(String)
    case chat(String)
    case newSession(spaceId: String)
}

struct HomeView: View {
    @Environment(AppModel.self) private var model
    @State private var path: [Route] = []
    @State private var showNewSpace = false
    // "" = All. Sticky across launches; falls back to All if the space is gone.
    @AppStorage("homeSpaceFilter") private var spaceFilter: String = ""

    private var selectedSpace: Space? {
        model.spaces.first { $0.id == spaceFilter }
    }

    var body: some View {
        NavigationStack(path: $path) {
            List {
                sessionsSection
                // The desktop's archived shelf sits under the active list,
                // scoped by the same space filter.
                ArchivedSection(spaceId: selectedSpace?.id, path: $path)
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle("Zeron")  // feeds the back menu; not displayed
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(removing: .title)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .space(let id): SpaceView(spaceId: id, path: $path)
                case .chat(let id): SessionView(chatId: id)
                case .newSession(let spaceId): NewSessionView(spaceId: spaceId, path: $path)
                }
            }
            .toolbar {
                // Let the native toolbar own this glass surface and its menu
                // morph. A second custom glass layer retains stale masks.
                ToolbarItem(placement: .topBarLeading) {
                    HStack(spacing: 10) {
                        spaceDropdown
                        // In the bar, not the list: as a list row it appeared
                        // and vanished with the connection and shoved the
                        // content down. Degraded states are GRACED (4s of
                        // continuous raw degradation before anything shows;
                        // recovery hides instantly) and quiet — a bare
                        // grayscale spinner or dot with a faint caption, no
                        // surface, no border (shell.rs render_connection_pill).
                        switch model.connectivity.state {
                        case .offline:
                            HStack(spacing: 5) {
                                Circle()
                                    .fill(Theme.warning)
                                    .frame(width: 5, height: 5)
                                Text("Offline — sends are saved")
                                    .font(Theme.sans(13))
                                    .foregroundStyle(Theme.textFaint)
                            }
                            .transition(.opacity)
                        case .reconnecting:
                            HStack(spacing: 5) {
                                ProgressView()
                                    .controlSize(.mini)
                                    .tint(Theme.textMuted)
                                Text("Reconnecting…")
                                    .font(Theme.sans(13))
                                    .foregroundStyle(Theme.textFaint)
                            }
                            .transition(.opacity)
                        case .connected:
                            // Initial catch-up (within the grace): the old
                            // quiet "connecting" spinner, gone on first sync.
                            if !model.connected {
                                ProgressView()
                                    .controlSize(.mini)
                                    .tint(Theme.textMuted)
                                    .accessibilityLabel("Connecting")
                            }
                        }
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    newButton
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        if model.demo != nil {
                            Text("Demo mode")
                        }
                        Button("Sign out", role: .destructive) { model.signOut() }
                    } label: {
                        Image(systemName: "person.circle")
                    }
                }
            }
            .sheet(isPresented: $showNewSpace) {
                NewSpaceSheet { spaceId in
                    path.append(.space(spaceId))
                }
            }
            .task(id: model.overviewChats.map(\.id).joined()) {
                model.preloadSessions()
            }
            .onAppear {
                if let route = model.launchRoute {
                    model.launchRoute = nil
                    // Push the whole stack atomically — appending from a child's
                    // onAppear mid-transition gets dropped by NavigationStack.
                    if case .space(let id) = route, model.launchSheet == "newsession" {
                        model.launchSheet = nil
                        path = [route, .newSession(spaceId: id)]
                    } else {
                        path = [route]
                    }
                }
                if model.launchSheet == "newspace" {
                    model.launchSheet = nil
                    showNewSpace = true
                }
            }
        }
    }

    // MARK: Space dropdown

    /// The nav-bar dropdown that scopes the session list — a NATIVE glass
    /// menu. Rows are Buttons, not a Picker: Picker menu rows drop two-Text
    /// subtitles, while Button rows map to UIAction subtitles, so each space
    /// shows its owning device ("@ mac") on the small second line without the
    /// three-line title wraps. Selection carries a checkmark in the icon slot.
    private var spaceDropdown: some View {
        Menu {
            spaceMenuButton(id: "", title: "All", subtitle: nil)
            ForEach(model.spaces) { space in
                spaceMenuButton(id: space.id, title: space.displayName,
                                subtitle: deviceTag(space))
            }
            Divider()
            Button {
                showNewSpace = true
            } label: {
                Label("New space…", systemImage: "folder.badge.plus")
            }
        } label: {
            HStack(spacing: 5) {
                Text(selectedSpace?.displayName ?? "All")
                    .font(Theme.sans(16, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Theme.textFaint)
            }
            // Keep long space names from swallowing the whole bar; the owning
            // device lives on the menu rows ("@ mac"), not up here.
            .frame(maxWidth: 200, alignment: .leading)
            .fixedSize(horizontal: true, vertical: false)
            .padding(.horizontal, 6)
            .frame(minHeight: 32)
        }
        // Put glass on the Menu control, not inside its captured label.
        // UIKit morphs the menu's own surface and can restore its new width
        // after selection; a label-level glass effect retained a stale mask.
        .buttonStyle(.glass)
        .buttonBorderShape(.capsule)
        .fixedSize(horizontal: true, vertical: false)
        .accessibilityLabel("Filter by space")
        .accessibilityIdentifier("space-filter")
    }

    private func deviceTag(_ space: Space) -> String {
        let name = model.deviceName(space.deviceId)
        return model.deviceOnline(space.deviceId) ? "@ \(name)" : "@ \(name) · offline"
    }

    private func spaceMenuButton(id: String, title: String, subtitle: String?) -> some View {
        let selected = id.isEmpty ? selectedSpace == nil : spaceFilter == id
        return Button {
            spaceFilter = id
        } label: {
            if selected {
                Label {
                    Text(title)
                    if let subtitle { Text(subtitle) }
                } icon: {
                    Image(systemName: "checkmark")
                }
            } else {
                Text(title)
                if let subtitle { Text(subtitle) }
            }
        }
    }

    /// "+" starts a session in the scoped space; under All it asks which
    /// space first. With no spaces yet it falls through to space creation.
    @ViewBuilder private var newButton: some View {
        if let space = selectedSpace {
            Button {
                path.append(.newSession(spaceId: space.id))
            } label: {
                Image(systemName: "plus")
            }
            .accessibilityLabel("New session")
        } else if model.spaces.isEmpty {
            Button {
                showNewSpace = true
            } label: {
                Image(systemName: "plus")
            }
            .accessibilityLabel("New space")
        } else {
            Menu {
                Section("New session in…") {
                    ForEach(model.spaces) { space in
                        Button {
                            path.append(.newSession(spaceId: space.id))
                        } label: {
                            // Button rows render the second Text as the
                            // subtitle line (same pattern as the space menu).
                            Text(space.displayName)
                            Text(deviceTag(space))
                        }
                    }
                }
            } label: {
                Image(systemName: "plus")
            }
            .accessibilityLabel("New session")
        }
    }

    // MARK: Sessions

    private var sessionsSection: some View {
        Section {
            let chats = selectedSpace.map { model.chats(in: $0.id) } ?? model.overviewChats
            if chats.isEmpty {
                Text(model.spaces.isEmpty
                    ? "No spaces yet — add one from a desktop device"
                    : "No sessions yet")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(chats) { chat in
                // Location shows even when scoped — without it the row's
                // first line is just a floating dot and a timestamp.
                ChatRow(chat: chat, showLocation: true) {
                    path.append(.chat(chat.id))
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .sessionPinAction(chat: chat, model: model)
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        // withAnimation, not a value-keyed .animation: the row
                        // leaves THIS section and lands in the archived shelf
                        // — one coordinated List diff, or the hand-off jumps.
                        withAnimation(Motion.resort) {
                            model.archive(chatId: chat.id)
                        }
                    } label: {
                        Label("Archive", systemImage: "archivebox")
                    }
                    .tint(Theme.surfaceRaised)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
        }
    }
}

// MARK: - Rows

/// The desktop session row (shell.rs `render_chat_row`), line for line: a
/// muted context line with the status word in the corner (dot + word, muted;
/// Done keeps its pop with a check; Idle rows carry the time-ago there
/// instead); the title on its own line; harness mark and branch close it out,
/// with the mini spinner riding the row's bottom-right while Working.
///
/// The one addition the phone needs: the desktop row names only the space
/// because its sidebar sits on the machine running the work. Here the Sessions
/// list interleaves every device, and a session whose host has gone offline
/// can't be driven at all — so the context line reads "space @ device".
struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool
    let onSelect: () -> Void

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    var body: some View {
        // The 1Hz pulse (live only while something is degraded or pending)
        // re-derives the send badge as its grace clocks advance.
        let _ = model.connectivity.pulse
        let indicator = model.indicator(for: chat)
        let sendState = model.sendState(for: chat)
        let pullRequest = model.changeRequest(for: chat)
        ZStack(alignment: .bottomTrailing) {
            Button(action: onSelect) {
                content(indicator: indicator, sendState: sendState,
                        reservesPullRequest: pullRequest != nil)
            }
            .buttonStyle(PressWashButtonStyle())
            if let pullRequest {
                PullRequestBadge(summary: pullRequest)
                    .padding(.trailing, 8)
                    .padding(.bottom, 6)
                    .zIndex(1)
            }
        }
    }

    /// Undelivered-send override for the corner slot (shell.rs precedence:
    /// Failed > Queued > the normal indicator). Suppresses the Working
    /// spinner too — no fake progress on a send that hasn't left.
    private func sendBadge(_ state: SendState) -> (label: String, color: Color)? {
        switch state {
        case .failed: return ("Failed", Theme.danger)
        case .queued: return ("Queued", Theme.warning)
        case .sending: return nil
        }
    }

    private func content(indicator: ChatIndicator, sendState: SendState?,
                         reservesPullRequest: Bool) -> some View {
        let badge = sendState.flatMap(sendBadge)
        return VStack(alignment: .leading, spacing: 2) {
            // Line 1: space @ device, status corner (time-ago when idle).
            HStack(spacing: 8) {
                if showLocation {
                    Text(location)
                        .font(Theme.sans(13))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Spacer(minLength: 4)
                }
                if let badge {
                    HStack(spacing: 4) {
                        Circle()
                            .fill(badge.color)
                            .frame(width: 6, height: 6)
                        Text(badge.label)
                            .font(Theme.sans(12, weight: .medium))
                            .foregroundStyle(badge.color)
                    }
                } else if indicator == .idle {
                    Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                        .font(Theme.sans(12, weight: .medium))
                        .foregroundStyle(subline)
                        .fixedSize()
                } else {
                    StatusCorner(indicator: indicator)
                }
            }

            // Line 2: the session title.
            HStack(spacing: 6) {
                if model.isPinned(chatId: chat.id) {
                    Image(systemName: "pin.fill")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(Theme.textMuted)
                }
                Text(chat.displayTitle)
                    .font(Theme.sans(17, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }

            // Line 3: harness brand mark, then the branch when the engine
            // stamped one; the Working spinner rides bottom-right.
            HStack(spacing: 4) {
                if let harness = chat.config?.harness {
                    HarnessBadge(harness: harness, size: 11, neutral: subline)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespaces), !branch.isEmpty {
                    LineIconView(.gitBranch, size: 11, color: subline)
                    Text(branch)
                        .font(Theme.sans(13))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 0)
                if indicator == .working, badge == nil {
                    MiniSpinner()
                }
            }
            .padding(.trailing, reservesPullRequest ? 46 : 0)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    /// "space @ device" (the session header's format). The space name (not
    /// the cwd basename) is what the desktop row shows — they differ once a
    /// space has been renamed, or when the session runs in a worktree off to
    /// the side. No offline marker: the dropdown carries device liveness.
    private var location: String {
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        return "\(space) @ \(model.deviceName(chat.deviceId))"
    }
}

extension View {
    func sessionPinAction(chat: Chat, model: AppModel) -> some View {
        swipeActions(edge: .leading, allowsFullSwipe: true) {
            let pinned = model.isPinned(chatId: chat.id)
            Button {
                withAnimation(Motion.resort) {
                    model.setPinned(chatId: chat.id, pinned: !pinned)
                }
            } label: {
                Label(pinned ? "Unpin" : "Pin", systemImage: pinned ? "pin.slash" : "pin")
            }
            .tint(Theme.accent)
            .disabled(!model.pinsReady)
        }
    }
}

func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
