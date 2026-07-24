// Space detail — the phone's answer to the desktop's horizontal session tabs:
// the space's sessions as a vertical list (creation order, like tab order),
// swipe-to-archive (= tab close), and "+" to start a session in this space.

import SwiftUI

struct SpaceView: View {
    @Environment(AppModel.self) private var model
    let spaceId: String
    @Binding var path: [Route]

    private var space: Space? {
        model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        List {
            let chats = model.chats(in: spaceId)
            if chats.isEmpty {
                emptyState
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    HStack(spacing: 10) {
                        HarnessBadge(harness: chat.config?.harness ?? "claude-code",
                                     size: 15, dimmed: true)
                        ChatRow(chat: chat, showLocation: false)
                    }
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
        }
        .listStyle(.plain)
        .environment(\.defaultMinListRowHeight, 10)
        .scrollContentBackground(.hidden)
        .scrollEdgeEffectStyle(.soft, for: .top)
        .background(Theme.surface.ignoresSafeArea())
        .navigationTitle(space?.displayName ?? "Space")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text(space?.displayName ?? "Space")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    if let space {
                        HStack(spacing: 4) {
                            Image(systemName: "folder")
                                .font(.system(size: 9))
                            Text("\(space.path) · \(model.deviceName(space.deviceId))")
                                .lineLimit(1)
                                .truncationMode(.head)
                        }
                        .font(Theme.sans(10.5))
                        .foregroundStyle(Theme.textMuted.opacity(0.6))
                    }
                }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    path.append(.newSession(spaceId: spaceId))
                } label: {
                    Image(systemName: "plus")
                }
                .accessibilityLabel("New session")
            }
        }
        .onAppear {
            if model.launchSheet == "newsession" {
                model.launchSheet = nil
                path.append(.newSession(spaceId: spaceId))
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 14) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text("No sessions in this space")
                .font(Theme.sans(13))
                .foregroundStyle(Theme.textFaint)
            Button {
                path.append(.newSession(spaceId: spaceId))
            } label: {
                Text("Start a session")
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .padding(.horizontal, 16)
                    .frame(height: 36)
            }
            .buttonStyle(.glass)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 48)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
    }

    private func shortPath(_ path: String) -> String {
        (path as NSString).lastPathComponent
    }
}


// MARK: - New space: remote folder browser

/// The desktop add-space palette translated to a sheet: device tabs, mono
/// breadcrumb with an up button, the device's folders (git repos badged),
/// "Use this folder" pinned at the bottom. Listing comes from the device over
/// the relay (ListFolders); dotfiles are pre-filtered and long listings are
/// truncated at 500 by the engine.
struct NewSpaceSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let onCreated: (String) -> Void

    @State private var deviceId: String?
    @State private var listing: FolderListing?
    @State private var loading = false
    @State private var error: String?
    @State private var creating = false

    private var devices: [DeviceRow] {
        // Engines own folders; this phone can't. Offer every other device.
        (model.demo?.devices ?? model.workspace?.devices ?? [])
            .filter { $0.platform != "ios" }
    }

    private var selectedDeviceId: String? {
        deviceId ?? devices.first?.id
    }

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 0) {
                if devices.isEmpty {
                    emptyDevices
                } else {
                    deviceTabs
                        .padding(.horizontal, 20)
                        .padding(.top, 6)
                        .padding(.bottom, 14)
                    breadcrumbBar
                        .padding(.horizontal, 20)
                        .padding(.bottom, 10)
                    folderList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(SheetStyle.panel)
            .navigationTitle("New space")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
            .safeAreaInset(edge: .bottom) {
                if !devices.isEmpty {
                    useFolderButton
                        .padding(.horizontal, 20)
                        .padding(.top, 8)
                        .padding(.bottom, 6)
                        .background(SheetStyle.panel.opacity(0.94))
                }
            }
        }
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
        .task(id: selectedDeviceId) {
            await load(path: nil)
        }
    }

    // MARK: Pieces

    private var deviceTabs: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(devices) { device in
                    let selected = device.id == selectedDeviceId
                    Button {
                        guard deviceId != device.id else { return }
                        UISelectionFeedbackGenerator().selectionChanged()
                        deviceId = device.id
                        listing = nil
                    } label: {
                        HStack(spacing: 7) {
                            Circle()
                                .fill(model.deviceOnline(device.id)
                                    ? Theme.statusCompleted.opacity(0.9) : whiteAlpha(0.18))
                                .frame(width: 6, height: 6)
                            Text(device.name)
                                .font(Theme.sans(13, weight: .medium))
                                .foregroundStyle(selected ? Theme.text : Theme.textMuted)
                        }
                        .padding(.horizontal, 14)
                        .frame(height: 36)
                        .background(selected ? whiteAlpha(0.15) : whiteAlpha(0.05), in: Capsule())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private var breadcrumbBar: some View {
        HStack(spacing: 10) {
            Button {
                if let parent = listing?.parent {
                    currentIsRepo = false
                    Task { await load(path: parent) }
                }
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(listing?.parent == nil ? Theme.textFaint.opacity(0.4) : Theme.text)
                    .frame(width: 32, height: 32)
                    .background(whiteAlpha(0.06), in: Circle())
            }
            .buttonStyle(.plain)
            .disabled(listing?.parent == nil)

            Text(listing?.path ?? " ")
                .font(Theme.mono(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.head)
                .frame(maxWidth: .infinity, alignment: .leading)

            if loading {
                ProgressView()
                    .controlSize(.mini)
                    .tint(Theme.textMuted)
            }
        }
    }

    private var folderList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                if let error {
                    Text(error)
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.danger)
                        .padding(.horizontal, 4)
                }
                let folders = (listing?.entries ?? []).filter(\.isDir)
                if folders.isEmpty, !loading, error == nil, listing != nil {
                    Text("No folders here")
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.textFaint)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 28)
                }
                if !folders.isEmpty {
                    SheetCard {
                        ForEach(Array(folders.enumerated()), id: \.element.name) { ix, entry in
                            folderRow(entry)
                            if ix < folders.count - 1 {
                                SheetSeparator()
                            }
                        }
                    }
                }
                if listing?.truncated == true {
                    Text("Listing truncated — this folder has more entries.")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                        .padding(.horizontal, 4)
                }
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 12)
        }
        .opacity(loading && listing == nil ? 0.4 : 1)
    }

    private func folderRow(_ entry: FolderEntry) -> some View {
        Button {
            guard let base = listing?.path else { return }
            let child = base.hasSuffix("/") ? base + entry.name : "\(base)/\(entry.name)"
            currentIsRepo = entry.isRepo
            Task { await load(path: child) }
        } label: {
            HStack(spacing: 12) {
                LineIconView(entry.isRepo ? .folderWithFiles : .folder, size: 16,
                             color: entry.isRepo ? Theme.accent.opacity(0.85) : Theme.textMuted)
                    .frame(width: 22)
                Text(entry.name)
                    .font(Theme.sans(15))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Spacer(minLength: 8)
                if entry.isRepo {
                    Text("git")
                        .font(Theme.mono(10))
                        .foregroundStyle(Theme.accent.opacity(0.85))
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Theme.accent.opacity(0.12), in: Capsule())
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

    private var useFolderButton: some View {
        let name = (listing?.path as NSString?)?.lastPathComponent ?? ""
        return SheetPrimaryButton(
            title: creating ? "Creating…" : (name.isEmpty ? "Use this folder" : "Use “\(name)”"),
            enabled: listing != nil && !creating && !loading
        ) {
            create()
        }
    }

    private var emptyDevices: some View {
        VStack(spacing: 12) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text("No devices yet")
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
            Text("Run Comet on a computer first — its folders will show up here.")
                .font(Theme.sans(13))
                .foregroundStyle(Theme.textMuted)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(32)
    }

    // MARK: Data

    private func load(path: String?) async {
        guard let selectedDeviceId else { return }
        loading = true
        error = nil
        let result = await model.listFolders(deviceId: selectedDeviceId, path: path)
        loading = false
        if let result {
            withAnimation(Motion.fadeQuick) {
                listing = result
            }
        } else if listing == nil {
            error = "Couldn't reach \(model.deviceName(selectedDeviceId)). Make sure it's online."
        }
    }

    private func create() {
        guard let selectedDeviceId, let listing else { return }
        creating = true
        // Initial git flag = the isRepo the engine stamped when we descended
        // into this folder; the owning device's SpacesSync re-verifies anyway.
        Task {
            let id = await model.createSpace(deviceId: selectedDeviceId,
                                             path: listing.path, gitDetected: currentIsRepo)
            creating = false
            if let id {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                dismiss()
                onCreated(id)
            }
        }
    }

    /// isRepo of the folder we're currently inside (set on descend; cleared
    /// when navigating up or switching device — unknowable there).
    @State private var currentIsRepo = false
}
