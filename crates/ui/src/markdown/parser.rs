//! Block-level markdown parsing over pulldown-cmark.
//!
//! Full parses build a [`BlockTree`] — a list of top-level blocks with their
//! byte ranges in the source. The streaming path ([`IncrementalParser`]) reparses
//! only from the last stable top-level block boundary: text before the start of
//! the last top-level block cannot be affected by an append, so each streamed
//! delta costs roughly O(delta + last block) instead of O(document).
//!
//! Soundness guard: link-reference definitions (`[label]: url`) have non-local
//! effects (a definition anywhere resolves references anywhere), so a source
//! containing one drops to full reparses. The parity unit tests stream corpora
//! through both paths and assert equality.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

/// Inline styling flags threaded through nested emphasis/links.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
    /// Destination URL when inside a link.
    pub link: Option<String>,
}

/// One run of identically-styled inline text.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineRun {
    pub text: String,
    pub style: InlineStyle,
}

/// A markdown block. Containers nest.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph {
        runs: Vec<InlineRun>,
    },
    Heading {
        level: u8,
        runs: Vec<InlineRun>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    BlockQuote {
        children: Vec<Block>,
    },
    List {
        ordered_start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Table {
        header: Vec<Vec<InlineRun>>,
        rows: Vec<Vec<Vec<InlineRun>>>,
    },
    Rule,
}

/// A top-level block plus its byte range in the source. The range start is the
/// stable-boundary anchor for incremental reparses.
#[derive(Debug, Clone, PartialEq)]
pub struct TopBlock {
    pub range: Range<usize>,
    pub block: Block,
}

/// The parse result: top-level blocks in document order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockTree {
    pub blocks: Vec<TopBlock>,
}

impl BlockTree {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }
}

// ---------------------------------------------------------------------------
// Full parse
// ---------------------------------------------------------------------------

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Parse a whole source into a [`BlockTree`].
pub fn parse_full(source: &str) -> BlockTree {
    let events: Vec<(Event, Range<usize>)> = Parser::new_ext(source, options())
        .into_offset_iter()
        .collect();
    let mut cur = Cursor {
        events: &events,
        ix: 0,
    };
    let mut blocks = Vec::new();
    while let Some((event, range)) = cur.peek() {
        let range = range.clone();
        match event {
            Event::Rule => {
                cur.bump();
                blocks.push(TopBlock {
                    range,
                    block: Block::Rule,
                });
            }
            Event::Start(_) => {
                for block in parse_started_block(&mut cur) {
                    blocks.push(TopBlock {
                        range: range.clone(),
                        block,
                    });
                }
            }
            // Stray inline events at top level (shouldn't happen): skip.
            _ => cur.bump(),
        }
    }
    BlockTree { blocks }
}

struct Cursor<'a, 'e> {
    events: &'a [(Event<'e>, Range<usize>)],
    ix: usize,
}

impl<'a, 'e> Cursor<'a, 'e> {
    fn peek(&self) -> Option<&(Event<'e>, Range<usize>)> {
        self.events.get(self.ix)
    }

    fn peek_event(&self) -> Option<&Event<'e>> {
        self.peek().map(|(e, _)| e)
    }

    fn bump(&mut self) {
        self.ix += 1;
    }

    fn next_event(&mut self) -> Option<Event<'e>> {
        let event = self.events.get(self.ix).map(|(e, _)| e.clone());
        if event.is_some() {
            self.ix += 1;
        }
        event
    }
}

fn is_block_tag(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::Table(_)
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
    )
}

/// Consume a `Start(tag)` and everything through its matching `End`, producing
/// block(s). Unknown containers are transparent (children splice in).
fn parse_started_block(cur: &mut Cursor) -> Vec<Block> {
    let Some(Event::Start(tag)) = cur.next_event() else {
        return Vec::new();
    };
    match tag {
        Tag::Paragraph => {
            vec![Block::Paragraph {
                runs: parse_inline_container(cur, &InlineStyle::default()),
            }]
        }
        Tag::Heading { level, .. } => vec![Block::Heading {
            level: heading_level(level),
            runs: parse_inline_container(cur, &InlineStyle::default()),
        }],
        Tag::CodeBlock(kind) => {
            let language = match kind {
                CodeBlockKind::Fenced(info) => {
                    let lang = info.split_whitespace().next().unwrap_or("");
                    if lang.is_empty() {
                        None
                    } else {
                        Some(lang.to_string())
                    }
                }
                CodeBlockKind::Indented => None,
            };
            let mut code = String::new();
            loop {
                match cur.next_event() {
                    Some(Event::Text(t)) => code.push_str(&t),
                    Some(Event::End(_)) | None => break,
                    Some(_) => {}
                }
            }
            // Fenced blocks carry a trailing newline; render per-line without it.
            if code.ends_with('\n') {
                code.pop();
            }
            vec![Block::CodeBlock { language, code }]
        }
        Tag::BlockQuote(_) => vec![Block::BlockQuote {
            children: parse_block_sequence(cur),
        }],
        Tag::List(ordered_start) => {
            let mut items = Vec::new();
            loop {
                match cur.peek_event() {
                    Some(Event::Start(Tag::Item)) => {
                        cur.bump();
                        items.push(parse_block_sequence(cur));
                    }
                    Some(Event::End(_)) | None => {
                        cur.bump();
                        break;
                    }
                    Some(_) => cur.bump(),
                }
            }
            vec![Block::List {
                ordered_start,
                items,
            }]
        }
        Tag::Table(_) => vec![parse_table(cur)],
        Tag::HtmlBlock => {
            // Render raw HTML blocks as plain text (comet's markdown does the same).
            let mut text = String::new();
            loop {
                match cur.next_event() {
                    Some(Event::Html(t)) | Some(Event::Text(t)) => text.push_str(&t),
                    Some(Event::End(_)) | None => break,
                    Some(_) => {}
                }
            }
            let text = text.trim_end_matches('\n').to_string();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph {
                    runs: vec![InlineRun {
                        text,
                        style: InlineStyle::default(),
                    }],
                }]
            }
        }
        // Transparent containers (footnote definitions when enabled, etc.).
        _ => parse_block_sequence(cur),
    }
}

/// Parse a block sequence until the container's `End` (consumed). Bare inline
/// events (tight list items) accumulate into an implicit paragraph.
fn parse_block_sequence(cur: &mut Cursor) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut inline_acc: Vec<InlineRun> = Vec::new();
    while let Some(event) = cur.peek_event() {
        match event {
            Event::End(_) => {
                cur.bump();
                break;
            }
            Event::Start(tag) if is_block_tag(tag) => {
                flush_paragraph(&mut out, &mut inline_acc);
                out.extend(parse_started_block(cur));
            }
            Event::Rule => {
                flush_paragraph(&mut out, &mut inline_acc);
                cur.bump();
                out.push(Block::Rule);
            }
            _ => parse_inline_event(cur, &mut inline_acc, &InlineStyle::default()),
        }
    }
    flush_paragraph(&mut out, &mut inline_acc);
    out
}

fn flush_paragraph(out: &mut Vec<Block>, acc: &mut Vec<InlineRun>) {
    if !acc.is_empty() {
        out.push(Block::Paragraph {
            runs: merge_runs(std::mem::take(acc)),
        });
    }
}

fn parse_table(cur: &mut Cursor) -> Block {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    loop {
        match cur.peek_event() {
            Some(Event::Start(Tag::TableHead)) => {
                cur.bump();
                header = parse_table_cells(cur);
            }
            Some(Event::Start(Tag::TableRow)) => {
                cur.bump();
                rows.push(parse_table_cells(cur));
            }
            Some(Event::End(_)) | None => {
                cur.bump();
                break;
            }
            Some(_) => cur.bump(),
        }
    }
    Block::Table { header, rows }
}

fn parse_table_cells(cur: &mut Cursor) -> Vec<Vec<InlineRun>> {
    let mut cells = Vec::new();
    loop {
        match cur.peek_event() {
            Some(Event::Start(Tag::TableCell)) => {
                cur.bump();
                cells.push(parse_inline_container(cur, &InlineStyle::default()));
            }
            Some(Event::End(_)) | None => {
                cur.bump();
                break;
            }
            Some(_) => cur.bump(),
        }
    }
    cells
}

/// Parse inline events until the container's `End` (consumed).
fn parse_inline_container(cur: &mut Cursor, style: &InlineStyle) -> Vec<InlineRun> {
    let mut runs = Vec::new();
    while let Some(event) = cur.peek_event() {
        if matches!(event, Event::End(_)) {
            cur.bump();
            break;
        }
        parse_inline_event(cur, &mut runs, style);
    }
    merge_runs(runs)
}

fn parse_inline_event(cur: &mut Cursor, runs: &mut Vec<InlineRun>, style: &InlineStyle) {
    let Some(event) = cur.next_event() else {
        return;
    };
    let push = |runs: &mut Vec<InlineRun>, text: String, style: InlineStyle| {
        if !text.is_empty() {
            runs.push(InlineRun { text, style });
        }
    };
    match event {
        Event::Text(t) => push(runs, t.into_string(), style.clone()),
        Event::Code(t) => {
            let mut s = style.clone();
            s.code = true;
            push(runs, t.into_string(), s);
        }
        Event::SoftBreak => push(runs, " ".into(), style.clone()),
        Event::HardBreak => push(runs, "\n".into(), style.clone()),
        Event::Html(t) | Event::InlineHtml(t) => push(runs, t.into_string(), style.clone()),
        Event::TaskListMarker(done) => push(
            runs,
            if done { "[x] ".into() } else { "[ ] ".into() },
            style.clone(),
        ),
        Event::FootnoteReference(t) => push(runs, format!("[{t}]"), style.clone()),
        Event::Start(tag) => {
            let mut inner = style.clone();
            match tag {
                Tag::Emphasis => inner.italic = true,
                Tag::Strong => inner.bold = true,
                Tag::Strikethrough => inner.strikethrough = true,
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    inner.link = Some(dest_url.into_string());
                }
                _ => {}
            }
            runs.extend(parse_inline_container(cur, &inner));
        }
        // `End` is consumed by the container loop; anything else is ignored.
        _ => {}
    }
}

/// Merge adjacent identically-styled runs (keeps run counts small and makes the
/// tree canonical for equality tests).
fn merge_runs(runs: Vec<InlineRun>) -> Vec<InlineRun> {
    let mut out: Vec<InlineRun> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(last) if last.style == run.style => last.text.push_str(&run.text),
            _ => out.push(run),
        }
    }
    out
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ---------------------------------------------------------------------------
// Incremental parse
// ---------------------------------------------------------------------------

/// Streaming parser: appends reparse only from the last stable top-level block
/// boundary (snapped back to a line start so indentation context survives).
#[derive(Debug, Default)]
pub struct IncrementalParser {
    source: String,
    tree: BlockTree,
    /// Link-reference definitions act at a distance — full reparses only.
    full_only: bool,
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn tree(&self) -> &BlockTree {
        &self.tree
    }

    /// Set the source: appends take the incremental path, anything else resets.
    pub fn set_text(&mut self, text: &str) {
        if text.len() >= self.source.len() && text.starts_with(self.source.as_str()) {
            let delta = text[self.source.len()..].to_string();
            self.append(&delta);
        } else {
            self.reset(text);
        }
    }

    pub fn reset(&mut self, text: &str) {
        self.source = text.to_string();
        self.full_only = has_link_defs(text);
        self.tree = parse_full(text);
    }

    /// Append streamed text, reparsing from the last stable boundary.
    pub fn append(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        // The delta may complete a line begun earlier — rescan from that line's
        // start when checking for definitions.
        let scan_from = self.source.rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.source.push_str(delta);
        if !self.full_only && has_link_defs(&self.source[scan_from..]) {
            self.full_only = true;
        }
        if self.full_only {
            self.tree = parse_full(&self.source);
            return;
        }

        // Stable boundary: start of the SECOND-to-last top-level block, snapped
        // back to its line start (keeps indented-code / fenced-indent context
        // intact). Reparsing the last two blocks — not just the last — covers
        // continuation merges: a trailing paragraph like `3` can become `3.`
        // and fuse into the preceding loose list. Merges cannot cascade
        // further back (a block's separation from its predecessor is decided
        // by its own already-streamed leading bytes), so two blocks suffice;
        // the parity tests stream corpora to hold this invariant.
        let boundary = match self.tree.blocks.len() {
            0 | 1 => 0,
            n => self.tree.blocks[n - 2].range.start,
        };
        let boundary = self.source[..boundary]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);

        let tail = parse_full(&self.source[boundary..]);
        self.tree.blocks.retain(|b| b.range.start < boundary);
        for mut top in tail.blocks {
            top.range.start += boundary;
            top.range.end += boundary;
            self.tree.blocks.push(top);
        }
    }
}

/// Conservative detector for link-reference-definition lines
/// (`[label]: destination`, up to 3 leading spaces).
fn has_link_defs(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        line.len() - trimmed.len() <= 3 && trimmed.starts_with('[') && trimmed.contains("]:")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(chunks: usize, text: &str) -> IncrementalParser {
        let mut p = IncrementalParser::new();
        let bytes = text.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            let mut end = (start + chunks).min(bytes.len());
            while end < bytes.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            p.append(&text[start..end]);
            start = end;
        }
        p
    }

    const CORPORA: &[&str] = &[
        "# Title\n\nHello **bold** and *italic* and `code` and ~~gone~~.\n",
        "Paragraph one\nlazy continuation\n\nParagraph two with a [link](https://x.dev).\n",
        "- item one\n- item two\n  - nested a\n  - nested b\n- item three\n\ntail\n",
        "1. first\n2. second\n\n   loose paragraph in item\n\n3. third\n",
        "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nafter code\n",
        "intro\n\n```\nunclosed fence streaming",
        "> quoted line\n> more quote\n>\n> - a list in a quote\n\nplain\n",
        "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ndone\n",
        "setext candidate\n===\n\nnext para\n---\n",
        "***\n\ntext between rules\n\n---\n",
        "- [x] done task\n- [ ] open task\n",
        "    indented code line one\n    line two\n\npara\n",
        "para with <span>inline html</span> inside\n\n<div>\nblock html\n</div>\n",
        "###### deep heading\n\n#### h4\n",
    ];

    #[test]
    fn incremental_matches_full_on_streamed_corpora() {
        for (ci, corpus) in CORPORA.iter().enumerate() {
            let full = parse_full(corpus);
            for chunk in [1usize, 2, 3, 7, 16, 64] {
                let inc = stream(chunk, corpus);
                assert_eq!(
                    inc.tree(),
                    &full,
                    "corpus {ci} diverged at chunk size {chunk}:\n{corpus}"
                );
            }
        }
    }

    #[test]
    fn incremental_matches_full_with_link_definitions() {
        // Definitions act at a distance → parser falls back to full reparses,
        // so parity must still hold.
        let corpus = "See [docs] for more.\n\nMore text.\n\n[docs]: https://example.com\n";
        let full = parse_full(corpus);
        for chunk in [1usize, 3, 9] {
            assert_eq!(stream(chunk, corpus).tree(), &full, "chunk {chunk}");
        }
        // The reference actually resolved into a link.
        let has_link = full.blocks.iter().any(|b| match &b.block {
            Block::Paragraph { runs } => runs.iter().any(|r| r.style.link.is_some()),
            _ => false,
        });
        assert!(has_link, "expected [docs] to resolve to a link");
    }

    #[test]
    fn set_text_appends_or_resets() {
        let mut p = IncrementalParser::new();
        p.set_text("hello");
        p.set_text("hello world");
        assert_eq!(p.tree(), &parse_full("hello world"));
        // Non-append rewrites reset cleanly.
        p.set_text("different");
        assert_eq!(p.tree(), &parse_full("different"));
        assert_eq!(p.source(), "different");
    }

    #[test]
    fn block_structure_basics() {
        let tree = parse_full("## Head\n\npara **b _bi_** text\n\n```ts\nlet x = 1;\n```\n");
        assert_eq!(tree.len(), 3);
        match &tree.blocks[0].block {
            Block::Heading { level, runs } => {
                assert_eq!(*level, 2);
                assert_eq!(runs[0].text, "Head");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &tree.blocks[1].block {
            Block::Paragraph { runs } => {
                assert_eq!(runs.len(), 4); // "para ", "b ", "bi" (bold+italic), " text"
                assert!(runs[1].style.bold && !runs[1].style.italic);
                assert!(runs[2].style.bold && runs[2].style.italic);
            }
            other => panic!("unexpected {other:?}"),
        }
        match &tree.blocks[2].block {
            Block::CodeBlock { language, code } => {
                assert_eq!(language.as_deref(), Some("ts"));
                assert_eq!(code, "let x = 1;");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nested_lists_and_tight_items() {
        let tree = parse_full("- a\n  - a1\n  - a2\n- b\n");
        let Block::List {
            ordered_start,
            items,
        } = &tree.blocks[0].block
        else {
            panic!("expected list");
        };
        assert_eq!(*ordered_start, None);
        assert_eq!(items.len(), 2);
        // Tight item text became an implicit paragraph, nested list follows.
        assert!(matches!(items[0][0], Block::Paragraph { .. }));
        assert!(matches!(items[0][1], Block::List { .. }));
    }

    #[test]
    fn tables_parse_header_and_rows() {
        let tree = parse_full("| a | b |\n|---|---|\n| 1 | 2 |\n");
        let Block::Table { header, rows } = &tree.blocks[0].block else {
            panic!("expected table");
        };
        assert_eq!(header.len(), 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1][0].text, "2");
    }

    #[test]
    fn links_carry_urls() {
        let tree = parse_full("go to [zed](https://zed.dev) now\n");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!()
        };
        let link = runs
            .iter()
            .find(|r| r.style.link.is_some())
            .expect("link run");
        assert_eq!(link.text, "zed");
        assert_eq!(link.style.link.as_deref(), Some("https://zed.dev"));
    }

    #[test]
    fn top_level_ranges_are_stable_anchors() {
        let src = "first\n\nsecond\n\nthird";
        let tree = parse_full(src);
        assert_eq!(tree.len(), 3);
        assert!(
            tree.blocks
                .windows(2)
                .all(|w| w[0].range.start < w[1].range.start)
        );
        assert_eq!(&src[tree.blocks[1].range.clone()], "second\n");
    }

    #[test]
    fn empty_and_whitespace_sources() {
        assert!(parse_full("").is_empty());
        assert!(parse_full("\n\n  \n").is_empty());
        let mut p = IncrementalParser::new();
        p.append("");
        assert!(p.tree().is_empty());
    }
}
