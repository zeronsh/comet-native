// Entity model — Swift mirrors of the workspace/session doc rows
// (crates/doc/src/workspace.rs, schema.rs) and the derived display state
// (crates/ui/src/state.rs, entities.rs). Field names match the doc schema
// exactly; derivations (indicator, staleness, attention rank) are ports.

import Foundation

// MARK: - Workspace doc rows

struct DeviceRow: Identifiable, Hashable {
    var id: String
    var name: String
    var platform: String
    var lastSeenAt: Int64?
    var createdAt: Int64?
}

struct Space: Identifiable, Hashable {
    var id: String
    var deviceId: String
    var path: String
    var name: String?
    var gitDetected: Bool
    var gitCheckedAt: Int64?
    var checkoutId: String?
    var createdAt: Int64

    /// Display name: explicit name, else the folder's basename.
    var displayName: String {
        if let name, !name.isEmpty { return name }
        return (path as NSString).lastPathComponent
    }
}

struct ChatConfig: Hashable, Codable {
    var harness: String
    var model: String?
    var reasoning: String?
    var sandbox: String?
}

struct Chat: Identifiable, Hashable {
    var id: String
    var deviceId: String
    var title: String?
    var archived: Bool
    var cwd: String?
    var branch: String?
    var checkoutId: String?
    var config: ChatConfig?
    var lastMessagePreview: String?
    var lastMessageAt: Int64?
    var createdAt: Int64
    var spaceId: String?
    var lastSeenAt: Int64?

    var displayTitle: String {
        if let title, !title.isEmpty { return title }
        return "New session"
    }

    /// entities.rs:123 — unseen when a message arrived after the last seen mark.
    var unseen: Bool {
        guard let lastMessageAt else { return false }
        guard let lastSeenAt else { return true }
        return lastMessageAt > lastSeenAt
    }
}

enum SessionStatus: String {
    case idle, working, awaitingInput, errored
}

struct SessionRow: Hashable {
    var chatId: String
    var deviceId: String
    var status: SessionStatus
    var startedAt: Int64?
    var updatedAt: Int64
}

// MARK: - Derived display status (entities.rs / state.rs ports)

enum ChatIndicator: Int {
    case awaitingInput = 0
    case errored = 1
    case working = 2
    case completed = 3
    case idle = 4
}

/// state.rs:277 — a Working/AwaitingInput row older than this reads as stale
/// (a crashed backend never shows eternal "Working").
let sessionStaleMs: Int64 = 45_000
/// workspace_host.rs:45 — presence freshness window for device online dots.
let presenceFreshMs: Int64 = 45_000

func effectiveStatus(_ row: SessionRow?, now: Int64) -> SessionStatus? {
    guard let row else { return nil }
    switch row.status {
    case .working, .awaitingInput:
        let age = now - row.updatedAt
        // Negative ages (clock skew) are fresh.
        return age > sessionStaleMs ? nil : row.status
    case .errored, .idle:
        return row.status
    }
}

/// entities.rs:147 — live Working/AwaitingInput win; Errored only if unseen;
/// else unseen ⇒ Completed; else Idle.
func chatIndicator(chat: Chat, live: SessionStatus?) -> ChatIndicator {
    switch live {
    case .working: return .working
    case .awaitingInput: return .awaitingInput
    case .errored: return chat.unseen ? .errored : .idle
    default: return chat.unseen ? .completed : .idle
    }
}

/// state.rs:311 — attention buckets, then recency, then id.
func attentionSort(_ chats: [Chat], indicator: (Chat) -> ChatIndicator) -> [Chat] {
    chats.sorted { a, b in
        let ra = indicator(a).rawValue, rb = indicator(b).rawValue
        if ra != rb { return ra < rb }
        let ta = a.lastMessageAt ?? a.createdAt, tb = b.lastMessageAt ?? b.createdAt
        if ta != tb { return ta > tb }
        return a.id < b.id
    }
}

// MARK: - Session doc entries

enum MessageRole: String {
    case user, assistant, system
}

enum MessageStatus: String {
    case streaming, complete, aborted
}

struct UserInputOption: Hashable, Codable {
    var label: String
    var description: String?
}

struct UserInputQuestion: Hashable, Codable {
    var id: String
    var header: String
    var question: String
    var options: [UserInputOption]
    var multiSelect: Bool?
}

struct UserInputAnswer: Hashable, Codable {
    var questionId: String
    var labels: [String]
}

/// Render-only sanitized tool call (packages render-parts policy).
struct RenderToolCall: Hashable {
    var tag: String
    /// Loose payload — only render-relevant fields survive in the doc.
    var fields: [String: AnyHashable]

    var string: (String) -> String? { { key in self.fields[key] as? String } }
}

enum MessagePart: Hashable, Identifiable {
    case text(id: String, text: String)
    case tool(id: String, call: RenderToolCall, isError: Bool, resolved: Bool)
    case input(id: String, requestId: String, questions: [UserInputQuestion], resolved: Bool)
    case error(id: String, message: String)

    var id: String {
        switch self {
        case .text(let id, _), .tool(let id, _, _, _), .input(let id, _, _, _), .error(let id, _):
            return id
        }
    }
}

struct MessageEntry: Identifiable, Hashable {
    var id: String
    var role: MessageRole
    var parts: [MessagePart]
    var createdAt: Int64
    var deviceId: String
    var status: MessageStatus?
    var continuationOf: String?
}

// MARK: - Folder browsing (add-space palette data)

/// comet-proto FolderListing (entities.rs:225): the device's answer to
/// ListFolders. Dotfiles are pre-filtered and entries are capped at 500 by
/// the engine; the parent path is computed client-side.
struct FolderEntry: Codable, Hashable {
    var name: String
    var isDir: Bool
    var isRepo: Bool
}

struct FolderListing: Codable {
    var path: String
    var entries: [FolderEntry]
    var truncated: Bool

    var parent: String? {
        guard path.contains("/"), path != "/" else { return nil }
        let trimmed = String(path[..<(path.lastIndex(of: "/") ?? path.startIndex)])
        return trimmed.isEmpty ? "/" : trimmed
    }
}

/// pickers.rs CheckoutKind — where a new session runs. "Current worktree" is
/// NOT a third mode: it's `local` when the picked ref is already materialized
/// as a worktree (the session reuses that checkout's path).
enum CheckoutKind {
    case local
    case newWorktree
}

/// comet-proto RepoRef (entities.rs:193): one selectable ref from ListRefs.
struct RepoRef: Codable, Hashable, Identifiable {
    var name: String
    var current: Bool = false
    var worktreePath: String?

    var id: String { name }
}

// MARK: - Command ledger (commands.rs port)

let commandDefaultTtlMs: Int64 = 86_400_000

/// comet-proto RunRequest (agent.rs:81). `reasoning` is lowercase
/// ("high"/"xhigh"/…), `sandbox` kebab-case ("workspace-write"), harness ids
/// kebab-case ("claude-code").
struct RunRequest: Codable {
    var prompt: String
    var model: String?
    var reasoning: String?
    var modelOptions: [String: String] = [:]
    var cwd: String
    var sandbox: String = "workspace-write"
    var autoApprove: Bool = true
    var resume: String?
}

enum SessionCommandPayload {
    case run(request: RunRequest, messageId: String)
    case steer(prompt: String, messageId: String?)
    case interrupt
    case respondInput(requestId: String, answers: [UserInputAnswer])

    var kind: String {
        switch self {
        case .run: return "run"
        case .steer: return "steer"
        case .interrupt: return "interrupt"
        case .respondInput: return "respondInput"
        }
    }
}

func nowMs() -> Int64 {
    Int64(Date().timeIntervalSince1970 * 1000)
}
