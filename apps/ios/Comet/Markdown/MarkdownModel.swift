// Markdown block model — a port of crates/ui/src/markdown/parser.rs.
//
// The transcript renders one row per *top-level block*, so the model is
// block-first: a parsed document is a flat list of `TopBlock`s whose content
// hash doubles as the row-version key for the virtualizer. Inline content is a
// run model (adjacent same-style runs merged) rather than an AST, which keeps
// rendering a single pass over styled spans.

import Foundation
import Markdown

struct InlineStyle: Hashable {
    var bold = false
    var italic = false
    var code = false
    var strikethrough = false
    var link: String? = nil

    static let plain = InlineStyle()
}

struct InlineRun: Hashable {
    var text: String
    var style: InlineStyle
}

enum MDAlign: Hashable {
    case left, center, right, none
}

struct MDListItem: Hashable {
    /// Task-list checkbox state; nil for plain items.
    var checked: Bool?
    var children: [MDBlock]
}

indirect enum MDBlock: Hashable {
    case paragraph([InlineRun])
    case heading(level: Int, [InlineRun])
    case codeBlock(language: String?, code: String)
    case blockquote([MDBlock])
    case list(orderedStart: Int?, items: [MDListItem])
    case table(header: [[InlineRun]], rows: [[[InlineRun]]], align: [MDAlign])
    case rule
}

/// A top-level block plus the 1-based source line it starts on (the stable
/// re-parse anchor) and a content hash used as the row diff key.
struct TopBlock: Hashable {
    var startLine: Int
    var block: MDBlock

    /// FNV-1a-style stable content fingerprint (row version key).
    var fingerprint: UInt64 {
        var hasher = Hasher()
        block.hash(into: &hasher)
        return UInt64(bitPattern: Int64(hasher.finalize()))
    }
}

// MARK: - AST walk (swift-markdown → block model)

enum MarkdownParser {
    /// Parse a complete markdown source into top-level blocks.
    static func parse(_ source: String) -> [TopBlock] {
        let document = Document(parsing: source)
        return document.children.compactMap { child in
            guard let block = convertBlock(child) else { return nil }
            let line = child.range?.lowerBound.line ?? 1
            return TopBlock(startLine: line, block: block)
        }
    }

    private static func convertBlock(_ markup: Markup) -> MDBlock? {
        switch markup {
        case let paragraph as Paragraph:
            return .paragraph(inlines(of: paragraph))
        case let heading as Heading:
            return .heading(level: heading.level, inlines(of: heading))
        case let code as CodeBlock:
            var body = code.code
            if body.hasSuffix("\n") { body.removeLast() }
            let lang = code.language.flatMap { $0.isEmpty ? nil : $0 }
            return .codeBlock(language: lang, code: body)
        case let quote as BlockQuote:
            return .blockquote(quote.children.compactMap(convertBlock))
        case let list as UnorderedList:
            return .list(orderedStart: nil, items: listItems(of: list))
        case let list as OrderedList:
            return .list(orderedStart: Int(list.startIndex), items: listItems(of: list))
        case let table as Markdown.Table:
            // `.cells`/`.rows` are lazy sequences — materialize eagerly.
            let header: [[InlineRun]] = table.head.cells.map { inlines(of: $0) }
            let rows: [[[InlineRun]]] = table.body.rows.map { row in
                row.cells.map { inlines(of: $0) } as [[InlineRun]]
            }
            let align: [MDAlign] = table.columnAlignments.map {
                switch $0 {
                case .left: return .left
                case .center: return .center
                case .right: return .right
                case nil: return .none
                }
            }
            return .table(header: header, rows: rows, align: align)
        case is ThematicBreak:
            return .rule
        case let html as HTMLBlock:
            // No HTML rendering — surface the raw source as a code block,
            // matching the desktop's plain-text fallback behavior.
            return .codeBlock(language: "html", code: html.rawHTML.trimmingCharacters(in: .newlines))
        default:
            // Unknown/unsupported block: flatten to a paragraph of its text.
            let text = markup.format()
            guard !text.isEmpty else { return nil }
            return .paragraph([InlineRun(text: text, style: .plain)])
        }
    }

    private static func listItems(of list: Markup) -> [MDListItem] {
        list.children.compactMap { child in
            guard let item = child as? ListItem else { return nil }
            let checked: Bool? = item.checkbox.map { $0 == .checked }
            return MDListItem(checked: checked, children: item.children.compactMap(convertBlock))
        }
    }

    private static func inlines(of container: Markup) -> [InlineRun] {
        var runs: [InlineRun] = []
        for child in container.children {
            collectInline(child, style: .plain, into: &runs)
        }
        return mergeRuns(runs)
    }

    private static func collectInline(_ markup: Markup, style: InlineStyle, into runs: inout [InlineRun]) {
        switch markup {
        case let text as Markdown.Text:
            runs.append(InlineRun(text: text.string, style: style))
        case let code as InlineCode:
            var s = style; s.code = true
            runs.append(InlineRun(text: code.code, style: s))
        case let strong as Strong:
            var s = style; s.bold = true
            strong.children.forEach { collectInline($0, style: s, into: &runs) }
        case let em as Emphasis:
            var s = style; s.italic = true
            em.children.forEach { collectInline($0, style: s, into: &runs) }
        case let strike as Strikethrough:
            var s = style; s.strikethrough = true
            strike.children.forEach { collectInline($0, style: s, into: &runs) }
        case let link as Markdown.Link:
            var s = style; s.link = link.destination
            link.children.forEach { collectInline($0, style: s, into: &runs) }
        case let image as Markdown.Image:
            // Images render as their alt text (desktop parity: no inline images).
            let alt = image.children.compactMap { ($0 as? Markdown.Text)?.string }.joined()
            runs.append(InlineRun(text: alt.isEmpty ? (image.source ?? "") : alt, style: style))
        case is SoftBreak:
            runs.append(InlineRun(text: " ", style: style))
        case is LineBreak:
            runs.append(InlineRun(text: "\n", style: style))
        case let html as InlineHTML:
            runs.append(InlineRun(text: html.rawHTML, style: style))
        default:
            for child in markup.children {
                collectInline(child, style: style, into: &runs)
            }
        }
    }

    /// Merge adjacent runs with identical style so rendering sees minimal spans.
    static func mergeRuns(_ runs: [InlineRun]) -> [InlineRun] {
        var merged: [InlineRun] = []
        for run in runs {
            if run.text.isEmpty { continue }
            if var last = merged.last, last.style == run.style {
                last.text += run.text
                merged[merged.count - 1] = last
            } else {
                merged.append(run)
            }
        }
        return merged
    }
}

// MARK: - Incremental streaming parser (parser.rs IncrementalParser port)

/// Re-parses only the streaming tail: on append, parsing restarts from the
/// start of the *second-to-last* top-level block (covers continuation merges),
/// so per-append cost is O(delta + tail), never O(document). Link-reference
/// definitions (`[label]: url`) break the locality assumption and force full
/// re-parses.
final class IncrementalMarkdownParser {
    private(set) var source: String = ""
    private(set) var blocks: [TopBlock] = []
    private var fullOnly = false

    func setText(_ text: String) {
        if text == source { return }
        if !fullOnly, text.hasPrefix(source), !source.isEmpty {
            append(text)
        } else {
            reset(text)
        }
    }

    private func reset(_ text: String) {
        source = text
        fullOnly = Self.hasLinkDefs(text)
        blocks = MarkdownParser.parse(text)
    }

    private func append(_ text: String) {
        let delta = String(text.dropFirst(source.count))
        source = text
        if Self.hasLinkDefs(delta) {
            fullOnly = true
            blocks = MarkdownParser.parse(text)
            return
        }
        guard blocks.count >= 2 else {
            blocks = MarkdownParser.parse(text)
            return
        }
        // Stable boundary: the start line of the second-to-last block.
        let boundaryLine = blocks[blocks.count - 2].startLine
        let stable = Array(blocks.prefix(blocks.count - 2))
        let tailSource = Self.suffix(of: text, fromLine: boundaryLine)
        let tailBlocks = MarkdownParser.parse(tailSource).map { top in
            TopBlock(startLine: top.startLine + boundaryLine - 1, block: top.block)
        }
        blocks = stable + tailBlocks
    }

    /// The substring starting at the given 1-based line.
    private static func suffix(of text: String, fromLine line: Int) -> String {
        guard line > 1 else { return text }
        var remaining = line - 1
        var index = text.startIndex
        while remaining > 0, let nl = text[index...].firstIndex(of: "\n") {
            index = text.index(after: nl)
            remaining -= 1
        }
        return String(text[index...])
    }

    private static let linkDefPattern = /(?m)^\s{0,3}\[[^\]]+\]:/
    static func hasLinkDefs(_ text: String) -> Bool {
        text.contains(linkDefPattern)
    }
}
