// Line-by-line syntax tokenizer — a port of crates/ui/src/markdown/highlight.rs.
//
// Paint-only: tokens recolor text runs on the same mono font, so highlighting
// can never change layout. Lines tokenize independently with a small carry
// state for block comments / multiline strings, which lets code blocks render
// line-per-row and lets highlighting arrive asynchronously without reflow.

import Foundation

enum TokenClass {
    case keyword
    case stringLit
    case comment
    case number
}

/// A classified span within a single line (character offsets).
struct TokenSpan {
    var range: Range<Int>
    var cls: TokenClass
}

enum HighlightLanguage: String {
    case rust, javascript, python, go, json, bash, toml, markdown, swift

    static func forTag(_ tag: String?) -> HighlightLanguage? {
        guard let tag = tag?.lowercased() else { return nil }
        switch tag {
        case "rust", "rs": return .rust
        case "js", "jsx", "ts", "tsx", "javascript", "typescript": return .javascript
        case "py", "python": return .python
        case "go", "golang": return .go
        case "json", "jsonc": return .json
        case "sh", "bash", "zsh", "shell", "console": return .bash
        case "toml": return .toml
        case "md", "markdown": return .markdown
        case "swift": return .swift
        default: return nil
        }
    }

    var keywords: Set<String> {
        switch self {
        case .rust:
            return ["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                    "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
                    "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
                    "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while"]
        case .javascript:
            return ["async", "await", "break", "case", "catch", "class", "const", "continue",
                    "default", "delete", "do", "else", "export", "extends", "false", "finally",
                    "for", "function", "if", "import", "in", "instanceof", "interface", "let",
                    "new", "null", "of", "return", "static", "super", "switch", "this", "throw",
                    "true", "try", "type", "typeof", "undefined", "var", "void", "while", "yield"]
        case .python:
            return ["and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                    "del", "elif", "else", "except", "False", "finally", "for", "from", "global",
                    "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass",
                    "raise", "return", "True", "try", "while", "with", "yield"]
        case .go:
            return ["break", "case", "chan", "const", "continue", "default", "defer", "else",
                    "fallthrough", "false", "for", "func", "go", "goto", "if", "import",
                    "interface", "map", "nil", "package", "range", "return", "select", "struct",
                    "switch", "true", "type", "var"]
        case .json:
            return ["true", "false", "null"]
        case .bash:
            return ["case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                    "if", "in", "local", "return", "then", "until", "while"]
        case .toml:
            return ["true", "false"]
        case .markdown:
            return []
        case .swift:
            return ["as", "async", "await", "break", "case", "catch", "class", "continue",
                    "default", "defer", "do", "else", "enum", "extension", "false", "final",
                    "for", "func", "guard", "if", "import", "in", "init", "internal", "is", "let",
                    "nil", "private", "protocol", "public", "return", "self", "Self", "static",
                    "struct", "switch", "throw", "throws", "true", "try", "var", "where", "while"]
        }
    }

    var lineComment: String? {
        switch self {
        case .rust, .javascript, .go, .swift: return "//"
        case .python, .bash, .toml: return "#"
        case .json, .markdown: return nil
        }
    }

    var blockComment: (open: String, close: String)? {
        switch self {
        case .rust, .javascript, .go, .swift: return ("/*", "*/")
        default: return nil
        }
    }

    var multilineString: String? {
        switch self {
        case .python: return "\"\"\""
        default: return nil
        }
    }
}

/// Carry state across lines (block comments / multiline strings).
struct LineCarry: Equatable {
    var inBlockComment = false
    var inMultilineString = false
}

enum Highlighter {
    /// Tokenize all lines of a code block. Pure; run off the main actor.
    static func highlight(code: String, language: HighlightLanguage) -> [[TokenSpan]] {
        var carry = LineCarry()
        return code.components(separatedBy: "\n").map { line in
            tokenizeLine(Array(line), language: language, carry: &carry)
        }
    }

    static func tokenizeLine(_ chars: [Character], language lang: HighlightLanguage, carry: inout LineCarry) -> [TokenSpan] {
        var spans: [TokenSpan] = []
        var i = 0
        let n = chars.count

        func matches(_ pattern: String, at index: Int) -> Bool {
            let p = Array(pattern)
            guard index + p.count <= n else { return false }
            for (k, ch) in p.enumerated() where chars[index + k] != ch { return false }
            return true
        }

        // Resume carry state.
        if carry.inBlockComment, let block = lang.blockComment {
            let start = i
            while i < n, !matches(block.close, at: i) { i += 1 }
            if i < n {
                i += block.close.count
                carry.inBlockComment = false
            } else {
                i = n
            }
            spans.append(TokenSpan(range: start..<i, cls: .comment))
        } else if carry.inMultilineString, let delim = lang.multilineString {
            let start = i
            while i < n, !matches(delim, at: i) { i += 1 }
            if i < n {
                i += delim.count
                carry.inMultilineString = false
            } else {
                i = n
            }
            spans.append(TokenSpan(range: start..<i, cls: .stringLit))
        }

        while i < n {
            let c = chars[i]

            // Comments
            if let block = lang.blockComment, matches(block.open, at: i) {
                let start = i
                i += block.open.count
                while i < n, !matches(block.close, at: i) { i += 1 }
                if i < n { i += block.close.count } else { carry.inBlockComment = true }
                spans.append(TokenSpan(range: start..<i, cls: .comment))
                continue
            }
            if let lc = lang.lineComment, matches(lc, at: i) {
                spans.append(TokenSpan(range: i..<n, cls: .comment))
                i = n
                continue
            }

            // Multiline string open (python triple-quote)
            if let delim = lang.multilineString, matches(delim, at: i) {
                let start = i
                i += delim.count
                while i < n, !matches(delim, at: i) { i += 1 }
                if i < n { i += delim.count } else { carry.inMultilineString = true }
                spans.append(TokenSpan(range: start..<i, cls: .stringLit))
                continue
            }

            // Strings
            if c == "\"" || c == "'" || (c == "`" && lang == .javascript) {
                let quote = c
                let start = i
                i += 1
                while i < n {
                    if chars[i] == "\\" { i += min(2, n - i); continue }
                    if chars[i] == quote { i += 1; break }
                    i += 1
                }
                spans.append(TokenSpan(range: start..<i, cls: .stringLit))
                continue
            }

            // Numbers
            if c.isNumber {
                let start = i
                while i < n, chars[i].isHexDigit || chars[i] == "." || chars[i] == "_"
                    || chars[i] == "x" || chars[i] == "o" || chars[i] == "b" || chars[i] == "e" {
                    i += 1
                }
                // Guard against identifiers that merely start with a digit-ish tail.
                if start == 0 || !(chars[start - 1].isLetter || chars[start - 1] == "_") {
                    spans.append(TokenSpan(range: start..<i, cls: .number))
                }
                continue
            }

            // Identifiers / keywords
            if c.isLetter || c == "_" {
                let start = i
                while i < n, chars[i].isLetter || chars[i].isNumber || chars[i] == "_" { i += 1 }
                let word = String(chars[start..<i])
                if lang.keywords.contains(word) {
                    spans.append(TokenSpan(range: start..<i, cls: .keyword))
                }
                continue
            }

            i += 1
        }
        return spans
    }
}
