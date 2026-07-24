// Transcript row model — a port of crates/ui/src/shell/transcript.rs
// rows_for_entry. One row = one markdown top-level block / tool group / chip,
// never one message: streamed tokens re-render one row, and SwiftUI's lazy
// stack only re-measures what changed.
//
// Stable ids: markdown rows are "{entryId}#{partId}.{blockIx}", tool groups
// "{entryId}#g{groupIx}", chips "{entryId}#{partId}". Live and completed parts
// split identically, so the live→complete handoff never changes row identity.

import Foundation

enum RowKind {
    case user(text: String)
    case markdown(block: MDBlock, streaming: Bool)
    case toolGroup(tools: [ToolItem], autoOpen: Bool)
    case inputChip(header: String, resolved: Bool)
    case errorChip(message: String)
}

struct ToolItem: Hashable {
    var call: RenderToolCall
    var isError: Bool
    var resolved: Bool
}

struct TranscriptRow: Identifiable {
    var id: String
    /// Content fingerprint — SwiftUI diff key; a changed version re-renders
    /// exactly one row.
    var version: UInt64
    var turnStart: Bool
    var kind: RowKind
    var entryId: String
    var timestamp: Int64?
}

enum TranscriptRowBuilder {
    /// Split entries into rows. `parsers` caches one incremental parser per
    /// "{entryId}#{partId}" so the streaming tail re-parses O(delta + tail).
    static func rows(entries: [MessageEntry],
                     pendingSends: [(messageId: String, text: String, at: Int64)],
                     parsers: inout [String: IncrementalMarkdownParser]) -> [TranscriptRow] {
        var rows: [TranscriptRow] = []
        for entry in entries {
            rowsForEntry(entry, into: &rows, parsers: &parsers)
        }
        // Optimistic echo: pending sends share their client-minted id, so the
        // host's real entry replaces them without a flicker.
        let ids = Set(entries.map(\.id))
        for pending in pendingSends where !ids.contains(pending.messageId) {
            rows.append(TranscriptRow(id: pending.messageId,
                                      version: fnv1a(pending.text) | 1,
                                      turnStart: true,
                                      kind: .user(text: pending.text),
                                      entryId: pending.messageId,
                                      timestamp: nil))
        }
        return rows
    }

    private static func rowsForEntry(_ entry: MessageEntry,
                                     into rows: inout [TranscriptRow],
                                     parsers: inout [String: IncrementalMarkdownParser]) {
        let streaming = entry.status == .streaming
        let settled = entry.status != nil && !streaming

        if entry.role == .user {
            // One bubble row per user message.
            let text = entry.parts.compactMap { part -> String? in
                if case .text(_, let t) = part { return t }
                return nil
            }.joined(separator: "\n")
            guard !text.isEmpty else { return }
            rows.append(TranscriptRow(id: entry.id, version: fnv1a(text),
                                      turnStart: true, kind: .user(text: text),
                                      entryId: entry.id, timestamp: entry.createdAt))
            return
        }

        var first = true
        var pendingTools: [ToolItem] = []
        var groupIx = 0
        let lastPartIx = entry.parts.indices.last

        func flushTools(lastIx: Int?) {
            guard !pendingTools.isEmpty else { return }
            let autoOpen = streaming && lastIx == lastPartIx
            let id = "\(entry.id)#g\(groupIx)"
            var version = toolFingerprint(pendingTools)
            if autoOpen { version ^= 1 }
            rows.append(TranscriptRow(id: id, version: version, turnStart: first,
                                      kind: .toolGroup(tools: pendingTools, autoOpen: autoOpen),
                                      entryId: entry.id, timestamp: nil))
            first = false
            pendingTools = []
            groupIx += 1
        }

        for (ix, part) in entry.parts.enumerated() {
            switch part {
            case .tool(_, let call, let isError, let resolved):
                pendingTools.append(ToolItem(call: call, isError: isError, resolved: resolved))
                if ix == lastPartIx { flushTools(lastIx: ix) }

            case .text(let partId, let text):
                flushTools(lastIx: ix - 1)
                guard !text.isEmpty else { continue }
                let key = "\(entry.id)#\(partId)"
                let isLiveTail = streaming && ix == lastPartIx
                let blocks = parse(text: text, key: key, streaming: isLiveTail, parsers: &parsers)
                for (blockIx, top) in blocks.enumerated() {
                    var version = (top.fingerprint << 1) | (isLiveTail && blockIx == blocks.count - 1 ? 1 : 0)
                    if settled, ix == lastPartIx, blockIx == blocks.count - 1 {
                        version ^= 1 << 62  // timestamp attach keeps the diff key honest
                    }
                    rows.append(TranscriptRow(
                        id: "\(key).\(blockIx)", version: version, turnStart: first,
                        kind: .markdown(block: top.block,
                                        streaming: isLiveTail && blockIx == blocks.count - 1),
                        entryId: entry.id,
                        timestamp: settled && ix == lastPartIx && blockIx == blocks.count - 1
                            ? entry.createdAt : nil))
                    first = false
                }

            case .input(let partId, _, let questions, let resolved):
                flushTools(lastIx: ix - 1)
                let header = questions.first?.header ?? "Question"
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)",
                                          version: fnv1a(header) | (resolved ? 1 : 0),
                                          turnStart: first,
                                          kind: .inputChip(header: header, resolved: resolved),
                                          entryId: entry.id, timestamp: nil))
                first = false

            case .error(let partId, let message):
                flushTools(lastIx: ix - 1)
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)", version: fnv1a(message),
                                          turnStart: first,
                                          kind: .errorChip(message: message),
                                          entryId: entry.id, timestamp: nil))
                first = false
            }
        }
        flushTools(lastIx: lastPartIx)
    }

    private static func parse(text: String, key: String, streaming: Bool,
                              parsers: inout [String: IncrementalMarkdownParser]) -> [TopBlock] {
        if streaming {
            let parser = parsers[key] ?? IncrementalMarkdownParser()
            parser.setText(text)
            parsers[key] = parser
            return parser.blocks
        }
        // Completed: adopt the live parser's tree when the flip happens
        // (handoff), else parse fresh; drop the live parser either way.
        if let live = parsers.removeValue(forKey: key), live.source == text {
            return live.blocks
        }
        return MarkdownParser.parse(text)
    }

    private static func toolFingerprint(_ tools: [ToolItem]) -> UInt64 {
        var hash: UInt64 = 0xcbf29ce484222325
        for tool in tools {
            for byte in tool.call.tag.utf8 {
                hash ^= UInt64(byte)
                hash = hash &* 0x100000001b3
            }
            hash ^= UInt64(tool.call.fields.count) &+ (tool.isError ? 2 : 0) &+ (tool.resolved ? 4 : 0)
            hash = hash &* 0x100000001b3
            for (k, v) in tool.call.fields.sorted(by: { $0.key < $1.key }) {
                for byte in "\(k)=\(v)".utf8 {
                    hash ^= UInt64(byte)
                    hash = hash &* 0x100000001b3
                }
            }
        }
        return hash << 3
    }

    static func fnv1a(_ text: String) -> UInt64 {
        var hash: UInt64 = 0xcbf29ce484222325
        for byte in text.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x100000001b3
        }
        return hash << 1
    }
}

// MARK: - Tool chip content (transcript.rs tool_chip_content_raw)

extension RenderToolCall {
    var chipLabel: String {
        switch tag {
        case "exec": return "Run"
        case "readFile": return "Read"
        case "writeFile": return "Write"
        case "editFile": return "Edit"
        case "applyPatch": return "Patch"
        case "search": return "Search"
        case "glob": return "Glob"
        case "webFetch": return "Fetch"
        case "webSearch": return "Web"
        case "todo": return "Todo"
        case "mcp": return "MCP"
        default: return "Tool"
        }
    }

    var chipDetail: String {
        switch tag {
        case "exec": return string("command") ?? ""
        case "readFile", "writeFile", "editFile": return shortPath(string("path") ?? "")
        case "applyPatch":
            let changes = (fields["changes"] as? [String])?.count ?? 0
            return changes == 1 ? "1 file" : "\(changes) files"
        case "search": return string("pattern") ?? ""
        case "glob": return string("pattern") ?? ""
        case "webFetch": return string("url") ?? ""
        case "webSearch": return string("query") ?? ""
        case "todo":
            return string("summary") ?? "task list"
        case "mcp":
            let server = string("server").map { "\($0) · " } ?? ""
            return server + (string("tool") ?? "")
        default: return string("name") ?? ""
        }
    }

    var chipSymbol: String {
        switch tag {
        case "exec": return "terminal"
        case "readFile", "applyPatch": return "doc.text"
        case "writeFile": return "doc.badge.plus"
        case "editFile": return "pencil"
        case "search": return "magnifyingglass"
        case "glob": return "folder"
        case "webFetch", "webSearch": return "globe"
        case "todo": return "checklist"
        default: return "square.grid.2x2"
        }
    }

    private func shortPath(_ path: String) -> String {
        let comps = path.split(separator: "/")
        guard comps.count > 2 else { return path }
        return comps.suffix(2).joined(separator: "/")
    }
}

/// "Ran 3 commands · edited 2 files · 1 failed" (transcript.rs
/// tool_group_summary).
func toolGroupSummary(_ tools: [ToolItem]) -> String {
    var segments: [String] = []
    let runs = tools.filter { $0.call.tag == "exec" }.count
    if runs > 0 { segments.append(runs == 1 ? "ran 1 command" : "ran \(runs) commands") }
    let edits = tools.filter { ["editFile", "writeFile", "applyPatch"].contains($0.call.tag) }.count
    if edits > 0 { segments.append(edits == 1 ? "edited 1 file" : "edited \(edits) files") }
    let reads = tools.filter { $0.call.tag == "readFile" }.count
    if reads > 0 { segments.append(reads == 1 ? "read 1 file" : "read \(reads) files") }
    let searches = tools.filter { ["search", "glob", "webSearch", "webFetch"].contains($0.call.tag) }.count
    if searches > 0 { segments.append(searches == 1 ? "1 search" : "\(searches) searches") }
    let other = tools.count - runs - edits - reads - searches
    if other > 0 { segments.append(other == 1 ? "1 tool" : "\(other) tools") }
    let failed = tools.filter(\.isError).count
    if failed > 0 { segments.append("\(failed) failed") }
    guard var summary = segments.first else { return "\(tools.count) tools" }
    summary = summary.prefix(1).uppercased() + summary.dropFirst()
    return ([summary] + segments.dropFirst()).joined(separator: " · ")
}
