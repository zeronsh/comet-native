// Archived shelf — the desktop sidebar's settled shelf for archived sessions
// (shell/spaces.rs `render_archived_section`), sitting under the active list:
// a hairline header that folds ("Archived" open / "Archived (N)" collapsed,
// open by default, session-transient), slim rows, and Show-more paging
// (10, then +25). The desktop's hover-swapped Unarchive pill becomes
// swipe-to-unarchive here, mirroring the active rows' swipe-to-archive.

import SwiftUI

private extension Chat {
    var shelfRowId: String { "archived-\(id)" }
}

struct ArchivedSection: View {
    @Environment(AppModel.self) private var model
    /// Scope, matching the list above it: nil = All.
    var spaceId: String?
    @Binding var path: [Route]

    // spaces.rs INITIAL/PAGE. Both session-transient, like the desktop's.
    @State private var open = true
    @State private var shown = ArchivedSection.initialCount
    private static let initialCount = 10
    private static let pageSize = 25

    private static let rowInsets = EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12)

    var body: some View {
        let archived = model.archivedChats(in: spaceId)
        if !archived.isEmpty {
            Section {
                header(count: archived.count)
                if open {
                    // Distinct identity namespace (desktop's "archived-{id}"
                    // vs "c:{id}" FLIP keys): the SAME id in both ForEach made
                    // SwiftUI animate archiving as a cross-section MOVE — the
                    // full-size row flew down through its neighbors and landed
                    // in the shelf before snapping to the slim style. With
                    // separate ids it's a clean exit + entrance.
                    ForEach(archived.prefix(shown), id: \.shelfRowId) { chat in
                        row(chat)
                    }
                    if archived.count > shown {
                        showMore(remaining: archived.count - shown)
                    }
                }
            }
        }
    }

    private func header(count: Int) -> some View {
        Button {
            withAnimation(Motion.collapse) {
                open.toggle()
                shown = Self.initialCount
            }
        } label: {
            HStack(spacing: 8) {
                Text(open ? "Archived" : "Archived (\(count))")
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(Theme.textMuted.opacity(0.5))
                    .fixedSize()
                Rectangle()
                    .fill(Theme.border.opacity(0.6))
                    .frame(height: 1)
                Image(systemName: open ? "chevron.down" : "chevron.right")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.textMuted.opacity(0.5))
            }
            .padding(.horizontal, 10)
            .padding(.top, 12)
            .padding(.bottom, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(open ? "Collapse archived" : "Expand archived, \(count) sessions")
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(Self.rowInsets)
    }

    /// Slim row: dimmed harness mark, muted title, time-ago (spaces.rs
    /// archived row — h 36, mark 14, title 13, time 11).
    private func row(_ chat: Chat) -> some View {
        Button {
            path.append(.chat(chat.id))
        } label: {
            HStack(spacing: 10) {
                if let harness = chat.config?.harness {
                    HarnessBadge(harness: harness, size: 14, dimmed: true)
                }
                Text(chat.displayTitle)
                    .font(Theme.sans(15))
                    .foregroundStyle(Theme.text.opacity(0.55))
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                    .font(Theme.sans(13))
                    .foregroundStyle(Theme.textMuted.opacity(0.55))
                    .fixedSize()
            }
            .padding(.horizontal, 10)
            .frame(minHeight: 44)
            .contentShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(PressWashButtonStyle())
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(Self.rowInsets)
        .swipeActions(edge: .trailing, allowsFullSwipe: true) {
            Button {
                withAnimation(Motion.resort) {
                    model.unarchive(chatId: chat.id)
                }
            } label: {
                Label("Unarchive", systemImage: "arrow.up.bin")
            }
            .tint(Theme.surfaceRaised)
        }
    }

    private func showMore(remaining: Int) -> some View {
        Button {
            shown = max(shown, Self.initialCount) + Self.pageSize
        } label: {
            HStack(spacing: 10) {
                Image(systemName: "plus")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Theme.textMuted.opacity(0.55))
                Text("Show \(min(remaining, Self.pageSize)) more")
                    .font(Theme.sans(15))
                    .foregroundStyle(Theme.textMuted.opacity(0.55))
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 10)
            .frame(minHeight: 44)
            .contentShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(PressWashButtonStyle())
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(Self.rowInsets)
    }
}
