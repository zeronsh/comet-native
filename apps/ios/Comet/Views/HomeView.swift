// Home — the mobile shell. The desktop sidebar's two sections become the
// phone's home screen: Spaces (grouped work) and Sessions (the global
// attention-sorted list). Tabs-as-sessions don't fit a phone; a space opens
// into its own session list instead, and close=archive becomes swipe-to-archive.

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

    var body: some View {
        NavigationStack(path: $path) {
            List {
                if !model.connected {
                    connectingNotice
                }
                spacesSection
                sessionsSection
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle("Comet")  // feeds the back menu; not displayed
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
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showNewSpace = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("New space")
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

    /// Shown only while disconnected — no chrome when everything is fine.
    private var connectingNotice: some View {
        HStack(spacing: 8) {
            ProgressView()
                .controlSize(.mini)
                .tint(Theme.textMuted)
            Text("Connecting to edge…")
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
    }

    // MARK: Spaces

    private var spacesSection: some View {
        Section {
            if model.spaces.isEmpty {
                Text("No spaces yet — add one from a desktop device")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(model.spaces) { space in
                Button {
                    path.append(.space(space.id))
                } label: {
                    SpaceRow(space: space)
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
            }
        } header: {
            sectionHeader("Spaces")
        }
    }

    // MARK: Sessions

    private var sessionsSection: some View {
        Section {
            let chats = model.overviewChats
            if chats.isEmpty {
                Text("No sessions yet")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        model.archive(chatId: chat.id)
                    } label: {
                        Label("Archive", systemImage: "archivebox")
                    }
                    .tint(Theme.surfaceRaised)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
        } header: {
            sectionHeader("Sessions")
        }
    }

    private func sectionHeader(_ title: String) -> some View {
        Text(title)
            .font(Theme.sans(11, weight: .medium))
            .foregroundStyle(Theme.textMuted.opacity(0.6))
            .textCase(nil)
            .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

// MARK: - Rows

struct SpaceRow: View {
    @Environment(AppModel.self) private var model
    let space: Space

    var body: some View {
        HStack(spacing: 8) {
            // Leading 6pt aggregate dot — position stable, most-urgent member.
            let agg = model.spaceIndicator(space.id)
            Circle()
                .fill((agg == .working || agg == .awaitingInput) ? (agg?.dotColor ?? whiteAlpha(0.14)) : whiteAlpha(0.14))
                .frame(width: 6, height: 6)
            Image(systemName: "folder")
                .font(.system(size: 13))
                .foregroundStyle(Theme.textMuted)
            Text(space.displayName)
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer(minLength: 8)
            deviceTag
            Image(systemName: "chevron.right")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Theme.textFaint.opacity(0.6))
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    private var deviceTag: some View {
        let online = model.deviceOnline(space.deviceId)
        let name = model.deviceName(space.deviceId)
        return Text(online ? "@ \(name)" : "@ \(name) · offline")
            .font(Theme.sans(12))
            .foregroundStyle(online ? Theme.textMuted.opacity(0.6) : Theme.warning.opacity(0.8))
            .lineLimit(1)
    }
}

struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool

    var body: some View {
        let indicator = model.indicator(for: chat)
        HStack(alignment: .top, spacing: 8) {
            StatusRail(indicator: indicator)
                .padding(.top, 4)
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(chat.displayTitle)
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted.opacity(0.5))
                }
                if showLocation {
                    Text(location)
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted.opacity(0.55))
                        .lineLimit(1)
                }
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    /// "folder · [branch ·] device", with offline marker (spaces.rs:414).
    private var location: String {
        var parts: [String] = []
        if let cwd = chat.cwd {
            parts.append((cwd as NSString).lastPathComponent)
        }
        if let branch = chat.branch, !branch.isEmpty {
            parts.append(branch)
        }
        let name = model.deviceName(chat.deviceId)
        parts.append(model.deviceOnline(chat.deviceId) ? name : "\(name) (offline)")
        return parts.joined(separator: " · ")
    }
}

func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
