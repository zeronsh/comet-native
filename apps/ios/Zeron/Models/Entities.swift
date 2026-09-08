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
    /// Engine version stamped by the device ("0.2.12"); gates the queued-
    /// attachment flow and other capability checks. nil reads as "too old".
    var version: String?
    /// Explicit feature declarations; unlike semver, these distinguish a
    /// personal integration build from upstream built at the same version.
    var capabilities: [String] = []

    func supports(_ capability: String) -> Bool {
        capabilities.contains(capability)
    }
}

enum EngineCapability {
    static let messageQueueV1 = "message-queue-v1"
    static let messageQueueActionsV1 = "message-queue-actions-v1"
    static let messageQueueAttachmentsV1 = "message-queue-attachments-v1"
    static let messageQueueCleanAttachmentTextV1 = "message-queue-clean-attachment-text-v1"
    static let messageQueueEditLeaseV1 = "message-queue-edit-lease-v1"
}

/// proto/src/lib.rs version_triple: parse a leading major.minor.patch,
/// tolerating a -suffix or +build on the patch. Anything else is nil —
/// version gates treat that as "too old".
func versionTriple(_ raw: String) -> (Int, Int, Int)? {
    let parts = raw.split(separator: ".", maxSplits: 2, omittingEmptySubsequences: false)
    guard parts.count == 3, let major = Int(parts[0]), let minor = Int(parts[1]) else { return nil }
    let digits = parts[2].prefix { $0.isNumber }
    guard !digits.isEmpty, let patch = Int(digits) else { return nil }
    let rest = parts[2].dropFirst(digits.count)
    guard rest.isEmpty || rest.first == "-" || rest.first == "+" else { return nil }
    return (major, minor, patch)
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
    /// Harness-specific option picks (option id → choice id, proto
    /// `ChatConfig.model_options`). Round-tripped so a mobile config edit
    /// never clobbers options the desktop pickers set — `setChatConfig`
    /// rewrites the whole `config` field under per-field LWW.
    var modelOptions: [String: JSONValue] = [:]
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
    /// Sync room generation (docs/chat2-sync.md M2): absent/1 = legacy s2
    /// (never dialed from mobile), 2 = chat2. The host flips it when seeding.
    var roomGen: Int? = nil

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

// MARK: - Checkout change requests

/// Provider-neutral lifecycle state from `zeron-proto`.
enum ChangeRequestState: String, Codable, Hashable {
    case open
    case closed
    case merged

    var label: String {
        switch self {
        case .open: return "Open"
        case .closed: return "Closed"
        case .merged: return "Merged"
        }
    }
}

struct ChangeRequestSummary: Codable, Hashable {
    var provider: String
    var number: UInt64
    var title: String
    var url: String
    var state: ChangeRequestState
    var baseRef: String
    var headRef: String
}

/// Latest successful host-side resolution. A nil `changeRequest` is an
/// authoritative successful lookup with no matching pull request.
struct CheckoutChangeRequestStatus: Codable, Hashable {
    var checkoutId: String
    var deviceId: String
    var cwd: String
    var branch: String
    var changeRequest: ChangeRequestSummary?
    var updatedAt: String
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

/// The Sessions list order: PURE RECENCY, id tiebreak — a port of state.rs
/// `sort_active`. Status drives the dot, never the position.
///
/// This used to bucket by attention first, which is what the desktop did
/// before 55e1845: opening a completed session marks it seen (completed →
/// idle), and the row then dropped a bucket out from under the pointer. The
/// dots carry urgency instead, so the order never moves on its own.
func sortActive(_ chats: [Chat]) -> [Chat] {
    chats.sorted { a, b in
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

struct UserInputQuestion: Hashable, Codable {
    var id: String
    var header: String
    var question: String
    /// Plain labels — `proto::agent::UserInputQuestion.options` is a
    /// `Vec<String>`. This was modelled as `{label, description}` objects,
    /// which NEVER decoded: every question arrived empty, so the panel had no
    /// options to show and an unresolved request crashed the app.
    var options: [String]
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

/// zeron-proto FolderListing (entities.rs:225): the device's answer to
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

/// zeron-proto RepoRef (entities.rs:193): one selectable ref from ListRefs.
struct RepoRef: Codable, Hashable, Identifiable {
    var name: String
    var current: Bool = false
    var worktreePath: String?

    var id: String { name }
}

// MARK: - Command ledger (commands.rs port)

let commandDefaultTtlMs: Int64 = 86_400_000

/// zeron-proto WorktreeSpec (agent.rs, PR #159): a worktree the HOST
/// materializes at command-drain time — the client never blocks on a
/// CreateWorktree relay RPC before a send. Old hosts ignore the field and run
/// in `cwd` (the repo's main checkout): degraded, never hung.
struct WorktreeSpec: Codable, Hashable {
    var repoPath: String
    var base: String
}

/// zeron-proto RunRequest (agent.rs:81). `reasoning` is lowercase
/// ("high"/"xhigh"/…), `sandbox` kebab-case ("workspace-write"), harness ids
/// kebab-case ("claude-code").
struct RunRequest: Codable {
    var prompt: String
    /// Harness id ("claude-code") picked at send time; rides the command so
    /// the host's claim-on-first-command records it even when the chat row is
    /// still syncing.
    var harness: String?
    var model: String?
    var reasoning: String?
    var modelOptions: [String: JSONValue] = [:]
    var cwd: String
    var sandbox: String = "workspace-write"
    var autoApprove: Bool = true
    var resume: String?
    /// Absolute paths of image attachments already staged on the run device
    /// (UploadChunk/UploadCommit) — or `pending://{uploadId}/{name}` refs on
    /// the queued flow (host ≥ 0.2.12), which the host resolves to absolute
    /// paths once the bytes land. The same refs ride the prompt text as
    /// `Attached images (local files …)` lines — this field additionally lets
    /// a harness inline the bytes as image content blocks.
    var attachments: [String] = []
    /// Worktree for the host to materialize at drain time (PR #159). Omitted
    /// from the JSON when nil, so old hosts see the legacy shape.
    var worktree: WorktreeSpec?
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
