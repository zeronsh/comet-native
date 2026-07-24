// Offline demo dataset — realistic spaces/sessions/transcripts so the app can
// be explored (and screenshotted) with no edge deployment. The flagship chat
// streams a reply on demand, exercising the live-row pipeline: incremental
// re-parse, veil fade-in, stick-to-bottom.

import Foundation
import Observation

@MainActor
@Observable
final class DemoDataset {
    var devices: [DeviceRow]
    var spaces: [Space]
    var chats: [Chat]
    var sessions: [String: SessionRow]
    private var stores: [String: SessionStore] = [:]
    private var streamTask: Task<Void, Never>?

    private static let dummyConfig = AppConfig(
        edgeURL: URL(string: "http://localhost:8787")!, mode: .dev,
        userId: "demo", orgId: "demo", deviceId: "ios-demo", deviceName: "iPhone")

    init(devices: [DeviceRow], spaces: [Space], chats: [Chat], sessions: [String: SessionRow]) {
        self.devices = devices
        self.spaces = spaces
        self.chats = chats
        self.sessions = sessions
    }

    static func standard() -> DemoDataset {
        let now = nowMs()
        let mac = DeviceRow(id: "dev-mac", name: "MacBook Pro", platform: "macos",
                            lastSeenAt: now, createdAt: now - 86_400_000 * 30)
        let vps = DeviceRow(id: "dev-vps", name: "hetzner-01", platform: "linux",
                            lastSeenAt: now - 600_000, createdAt: now - 86_400_000 * 12)
        let comet = Space(id: "space-comet", deviceId: "dev-mac",
                          path: "/Users/dev/comet-native", name: nil, gitDetected: true,
                          gitCheckedAt: now, checkoutId: nil, createdAt: now - 86_400_000 * 9)
        let edge = Space(id: "space-edge", deviceId: "dev-vps",
                         path: "/srv/deploys/edge", name: nil, gitDetected: true,
                         gitCheckedAt: now, checkoutId: nil, createdAt: now - 86_400_000 * 4)

        let claude = ChatConfig(harness: "claude-code", model: "claude-fable-5",
                                reasoning: "xhigh", sandbox: "workspace-write")
        let codex = ChatConfig(harness: "codex", model: "gpt-5.6-terra",
                               reasoning: "high", sandbox: "workspace-write")

        let chats = [
            Chat(id: "chat-veil", deviceId: "dev-mac", title: "Streaming veil on transcript rows",
                 archived: false, cwd: "/Users/dev/.comet-native/worktrees/comet-native-veil-fade",
                 branch: "veil-fade", checkoutId: nil,
                 config: claude, lastMessagePreview: "Porting the paint-only fade…",
                 lastMessageAt: now - 40_000, createdAt: now - 3_600_000,
                 spaceId: comet.id, lastSeenAt: now),
            Chat(id: "chat-picker", deviceId: "dev-mac", title: "Model picker catalog sync",
                 archived: false, cwd: comet.path, branch: "main", checkoutId: nil,
                 config: claude, lastMessagePreview: "Which device owns the catalog?",
                 lastMessageAt: now - 120_000, createdAt: now - 7_200_000,
                 spaceId: comet.id, lastSeenAt: now - 130_000),
            Chat(id: "chat-tabs", deviceId: "dev-mac", title: "Tool group header colors",
                 archived: false, cwd: comet.path, branch: "main", checkoutId: nil,
                 config: codex, lastMessagePreview: "Done — failed children stay quiet.",
                 lastMessageAt: now - 900_000, createdAt: now - 86_400_000,
                 spaceId: comet.id, lastSeenAt: now - 3_600_000),
            Chat(id: "chat-deploy", deviceId: "dev-vps", title: "Wrangler deploy hygiene",
                 archived: false, cwd: edge.path, branch: nil, checkoutId: nil,
                 config: claude, lastMessagePreview: "Hibernation-safe flush timer",
                 lastMessageAt: now - 86_400_000, createdAt: now - 86_400_000 * 2,
                 spaceId: edge.id, lastSeenAt: now - 86_400_000),
        ]
        let sessions: [String: SessionRow] = [
            "chat-veil": SessionRow(chatId: "chat-veil", deviceId: "dev-mac", status: .working,
                                    startedAt: now - 95_000, updatedAt: now - 5_000),
            "chat-picker": SessionRow(chatId: "chat-picker", deviceId: "dev-mac",
                                      status: .awaitingInput, startedAt: now - 400_000,
                                      updatedAt: now - 10_000),
        ]
        return DemoDataset(devices: [mac, vps], spaces: [comet, edge],
                           chats: chats, sessions: sessions)
    }

    // MARK: Fake filesystem (folder browser demo)

    static let fileTree: [String: [String]] = [
        "/Users/dev": ["Documents", "Downloads", "Projects", "scratch"],
        "/Users/dev/Documents": ["notes", "specs"],
        "/Users/dev/Projects": ["comet-native", "dotfiles", "blog", "playground"],
        "/Users/dev/Projects/comet-native": ["apps", "crates", "docs", "edge"],
        "/Users/dev/Projects/blog": ["content", "public"],
        "/srv": ["deploys", "backups"],
        "/srv/deploys": ["edge", "landing"],
    ]

    func homePath(deviceId: String) -> String {
        deviceId == "dev-vps" ? "/srv" : "/Users/dev"
    }

    private static let repoNames: Set<String> = ["comet-native", "dotfiles", "blog", "playground", "edge", "landing"]

    func listFolders(deviceId: String, path: String) -> FolderListing {
        let entries = (Self.fileTree[path] ?? []).map { name in
            FolderEntry(name: name, isDir: true, isRepo: Self.repoNames.contains(name))
        }
        return FolderListing(path: path, entries: entries, truncated: false)
    }

    private var refsByPath: [String: [RepoRef]] = [:]

    func listRefs(spacePath: String) -> [RepoRef] {
        if let cached = refsByPath[spacePath] { return cached }
        let seeded: [RepoRef]
        if spacePath.contains("comet-native") {
            seeded = [
                RepoRef(name: "main", current: true, worktreePath: nil),
                RepoRef(name: "veil-fade", current: false,
                        worktreePath: "/Users/dev/.comet-native/worktrees/comet-native-veil-fade"),
                RepoRef(name: "feature/diff-pane", current: false, worktreePath: nil),
                RepoRef(name: "fix/tool-colors", current: false, worktreePath: nil),
            ]
        } else {
            seeded = [
                RepoRef(name: "main", current: true, worktreePath: nil),
                RepoRef(name: "staging", current: false, worktreePath: nil),
            ]
        }
        refsByPath[spacePath] = seeded
        return seeded
    }

    /// git checkout simulation: move the `current` marker in the repo at path.
    func switchRef(path: String, refName: String) {
        var refs = listRefs(spacePath: path)
        for ix in refs.indices {
            refs[ix].current = refs[ix].name == refName
        }
        refsByPath[path] = refs
    }

    func createWorktree(spacePath: String, base: String) -> String {
        let slug = base.replacingOccurrences(of: "/", with: "-")
        let path = "/Users/dev/.comet-native/worktrees/\((spacePath as NSString).lastPathComponent)-\(slug)"
        var refs = listRefs(spacePath: spacePath)
        if let ix = refs.firstIndex(where: { $0.name == base }), refs[ix].worktreePath == nil {
            refs[ix].worktreePath = path
        }
        refsByPath[spacePath] = refs
        return path
    }

    func sessionStore(for chatId: String) -> SessionStore {
        if let existing = stores[chatId] { return existing }
        let store = SessionStore(chatId: chatId, config: Self.dummyConfig, offline: true)
        store.setEntries(Self.transcript(for: chatId))
        store.demoResponder = { [weak self, weak store] prompt in
            guard let self, let store else { return }
            self.simulateTurn(store: store, chatId: chatId, prompt: prompt)
        }
        stores[chatId] = store
        return store
    }

    // MARK: Scripted transcripts

    private static func transcript(for chatId: String) -> [MessageEntry] {
        let now = nowMs()
        switch chatId {
        case "chat-veil":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Port the streaming fade-in veil from the desktop transcript. It must never affect layout — opacity only, split at chunk boundaries."),
                ], createdAt: now - 3_500_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: """
                    ## Veil port plan

                    The desktop veil (`veil.rs`) multiplies a fading alpha into each appended \
                    chunk's text color — **paint-layer only**, so shaping and wrapping never change. \
                    Three invariants to carry over:

                    1. Chunk spans keep their *exact* byte length when split
                    2. Fade duration tracks the append cadence: `clamp(ema × 3, 120, 400)` ms
                    3. Re-attach seeds the baseline — only post-switch appends animate

                    | Constant | Value |
                    | --- | --- |
                    | `VEIL_MIN_FADE_MS` | 120 |
                    | `VEIL_MAX_FADE_MS` | 400 |
                    | `VEIL_CURVE_POW` | 1.6 |

                    > The curve is `1 − (1−p)^1.6` — fast attack, soft landing.
                    """),
                    .tool(id: "tool1", call: RenderToolCall(tag: "readFile", fields: ["path": "crates/ui/src/markdown/veil.rs"]), isError: false, resolved: true),
                    .tool(id: "tool2", call: RenderToolCall(tag: "editFile", fields: ["path": "Comet/Transcript/Veil.swift"]), isError: false, resolved: true),
                    .tool(id: "tool3", call: RenderToolCall(tag: "exec", fields: ["command": "xcodebuild -scheme Comet build"]), isError: false, resolved: true),
                    .text(id: "t1", text: """
                    Implementation lands in `Veil.swift`:

                    ```swift
                    func veilOpacity(_ p: Double) -> Double {
                        1 - pow(1 - p, 1.6)  // fast attack, soft landing
                    }

                    // Duration follows the streaming cadence EMA.
                    let duration = min(max(ema * 3, 120), 400)
                    ```

                    The row keeps one `RowVeil` while streaming and drops it on the \
                    live→complete flip, exactly like the desktop lifecycle.
                    """),
                ], createdAt: now - 3_400_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-picker":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "The model picker shows stale catalogs after switching devices — where should the catalog come from?"),
                ], createdAt: now - 400_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: "Two viable sources — the local device's harness install, or the space's owning device. The desktop recently moved to the latter (`aa128a6`). Before I wire the RPC, one decision:"),
                    .input(id: "req-1", requestId: "req-1", questions: [
                        UserInputQuestion(id: "q1", header: "Catalog source",
                                          question: "Which device should serve harness/model catalogs for the picker?",
                                          options: [
                                            UserInputOption(label: "Space's device (Recommended)",
                                                            description: "Catalogs come from the device that will run the session — always accurate."),
                                            UserInputOption(label: "Local device",
                                                            description: "Faster, but wrong when the space lives elsewhere."),
                                            UserInputOption(label: "Union of both",
                                                            description: "Show everything, validate at send time."),
                                          ], multiSelect: false),
                    ], resolved: false),
                ], createdAt: now - 380_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-tabs":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Tool group headers turn red when any child fails — they should stay quiet, chips carry the error."),
                ], createdAt: now - 1_000_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .tool(id: "tool1", call: RenderToolCall(tag: "search", fields: ["pattern": "group_header_color"]), isError: false, resolved: true),
                    .tool(id: "tool2", call: RenderToolCall(tag: "exec", fields: ["command": "cargo test -p comet-ui tool_group"]), isError: true, resolved: true),
                    .tool(id: "tool3", call: RenderToolCall(tag: "editFile", fields: ["path": "crates/ui/src/shell/transcript.rs"]), isError: false, resolved: true),
                    .text(id: "t0", text: "Done — the header keeps `text_muted` even on failure; only the chip label and the summary segment (\"1 failed\") pick up `danger`. Matches the desktop fix in `1749890`."),
                ], createdAt: now - 950_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-deploy":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Audit the wrangler config for hibernation hygiene."),
                ], createdAt: now - 86_500_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: "Flush timer now only arms while dirty; ping/pong uses the auto-response path so the DO never wakes for keepalives."),
                ], createdAt: now - 86_400_000, deviceId: "dev-vps", status: .complete, continuationOf: nil),
            ]
        default:
            return []  // freshly minted chats start empty
        }
    }

    // MARK: Streaming simulation

    private func simulateTurn(store: SessionStore, chatId: String, prompt: String) {
        streamTask?.cancel()
        let now = nowMs()
        var entries = store.entries
        entries.append(MessageEntry(id: "u-\(now)", role: .user, parts: [
            .text(id: "t0", text: prompt),
        ], createdAt: now, deviceId: "ios-demo", status: .complete, continuationOf: nil))
        let liveId = "a-\(now)"
        entries.append(MessageEntry(id: liveId, role: .assistant, parts: [
            .text(id: "t0", text: ""),
        ], createdAt: now, deviceId: "dev-mac", status: .streaming, continuationOf: nil))
        store.setEntries(entries)
        sessions[chatId] = SessionRow(chatId: chatId, deviceId: "dev-mac", status: .working,
                                      startedAt: now, updatedAt: now)

        let reply = """
        Here's how the streamed reply renders on this device:

        - Markdown re-parses **only the tail** — the last two top-level blocks
        - New text fades in through the paint-only veil
        - The transcript stays glued to the bottom until you scroll up

        ```rust
        // The desktop constant carries over verbatim.
        const STREAM_COMMIT_MS: u64 = 120;
        ```

        When the turn settles, this entry flips `streaming → complete`, the veil \
        drops, and the row ids stay stable so nothing flickers.
        """
        let words = reply.split(separator: " ", omittingEmptySubsequences: false)

        streamTask = Task { [weak self, weak store] in
            var text = ""
            for (ix, word) in words.enumerated() {
                if Task.isCancelled { return }
                text += (ix == 0 ? "" : " ") + word
                guard let store else { return }
                var current = store.entries
                guard let last = current.indices.last, current[last].id == liveId else { return }
                current[last].parts = [.text(id: "t0", text: text)]
                store.setEntries(current)
                try? await Task.sleep(nanoseconds: UInt64.random(in: 30_000_000...140_000_000))
            }
            guard let self, let store else { return }
            var current = store.entries
            if let last = current.indices.last, current[last].id == liveId {
                current[last].status = .complete
                store.setEntries(current)
            }
            let end = nowMs()
            self.sessions[chatId] = SessionRow(chatId: chatId, deviceId: "dev-mac", status: .idle,
                                               startedAt: nil, updatedAt: end)
            if let ix = self.chats.firstIndex(where: { $0.id == chatId }) {
                self.chats[ix].lastMessageAt = end
                self.chats[ix].lastMessagePreview = "When the turn settles, this entry flips…"
                self.chats[ix].lastSeenAt = end
            }
        }
    }
}
