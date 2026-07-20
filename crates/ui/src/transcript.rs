//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups and
//!   input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries stay UNSPLIT (one row per text part) and re-split on
//!   completion; the first split block reuses the live row's id, so row identity
//!   is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom uses gpui's `FollowMode::Tail` (wheel-up breaks the pin
//! inside the list's own input path) plus our 70px re-engage band in the scroll
//! handler; own-send re-engages with a smooth animated scroll.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, FollowMode, ListAlignment, ListScrollEvent, ListState,
    SharedString, Subscription, Task, Window, div, list, prelude::*, px,
};

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::ToolCall;

use crate::markdown::highlight::{Lang, LineCarry, Token, lang_for_tag, tokenize_line};
use crate::markdown::parser::{Block, BlockTree, IncrementalParser, parse_full};
use crate::markdown::render::{self, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
pub const OVERDRAW_PX: f32 = 320.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Vertical gap opening a new turn (new message entry).
pub const GAP_TURN: f32 = 14.0;
/// Vertical gap between blocks within a turn.
pub const GAP_BLOCK: f32 = 8.0;
/// Transcript column max width (comet 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// Tool chip row height / gap — analytic, so fold heights need no measurement.
/// A row is the guide rail + a 30px chip card centered in it (comet
/// tool-chip.tsx: `TOOL_CHIP_HEIGHT = 38`, card `h-[30px]`); rows stack with no
/// gap so the rail reads continuous.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
const CHIPS_TOP_PAD: f32 = 2.0;

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
}

#[derive(Clone)]
pub enum RowKind {
    User {
        text: SharedString,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// A whole streaming text part, unsplit (boundaries shift while streaming).
    LiveMarkdown {
        tree: Arc<BlockTree>,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        questions: usize,
        resolved: bool,
    },
    ErrorChip {
        message: SharedString,
    },
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: SharedString,
    pub version: u64,
    /// First row of its message entry (gets the turn gap).
    pub turn_start: bool,
    pub kind: RowKind,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

fn tool_fingerprint(tools: &[ToolItem], auto_open: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for t in tools {
        let (label, detail) = tool_chip_content(&t.call);
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(&(detail.len() as u32).to_le_bytes());
        acc.push(t.is_error as u8 | (t.resolved as u8) << 1);
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Build the block rows of one (already continuation-joined) entry.
///
/// `parse` maps `(part_key, text)` to a block tree — the entity supplies
/// incremental parsers for live parts and a cache for complete ones; tests pass
/// a plain `parse_full`.
pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);

    if entry.role == MessageRole::User {
        let text: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return vec![Row {
            id: entry.id.clone().into(),
            version: (text.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                text: text.into(),
                pending,
            },
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive tools.
    let last_part_ix = entry.parts.len().saturating_sub(1);
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let auto_open = streaming && last_ix == last_part_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix).into(),
                version: tool_fingerprint(&tools, auto_open),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    auto_open,
                },
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                ..
            } => {
                pending_group.push(ToolItem {
                    call: call.clone(),
                    is_error: *is_error,
                    resolved: *resolved,
                });
                group_last_part_ix = part_ix;
            }
            other => {
                flush_group(
                    &mut rows,
                    &mut pending_group,
                    &mut group_ix,
                    group_last_part_ix,
                );
                match other {
                    MessagePart::Text { id: part_id, text } => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, text);
                        if streaming {
                            // Live turn stays unsplit — one row, id matching the
                            // eventual first split block for flicker-free handoff.
                            rows.push(Row {
                                id: format!("{key}.0").into(),
                                version: (text.len() as u64) << 1 | 1,
                                turn_start: false,
                                kind: RowKind::LiveMarkdown { tree },
                            });
                        } else {
                            for block_ix in 0..tree.blocks.len() {
                                let end = tree.blocks[block_ix].range.end.min(text.len());
                                rows.push(Row {
                                    id: format!("{key}.{block_ix}").into(),
                                    version: (end as u64) << 1,
                                    turn_start: false,
                                    kind: RowKind::Markdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    },
                                });
                            }
                        }
                    }
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: (questions.len() as u64) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                questions: questions.len(),
                                resolved: *resolved,
                            },
                        });
                    }
                    MessagePart::Error {
                        id: part_id,
                        message,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: message.len() as u64,
                            turn_start: false,
                            kind: RowKind::ErrorChip {
                                message: message.clone().into(),
                            },
                        });
                    }
                    // Tools are grouped by the outer arm; nothing reaches here.
                    MessagePart::Tool { .. } => {}
                }
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    rows
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel; the block gap otherwise.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return GAP_TURN;
    }
    let same_part_markdown = prev.is_some_and(|p| {
        matches!(p.kind, RowKind::Markdown { .. })
            && matches!(row.kind, RowKind::Markdown { .. })
            && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else {
        GAP_BLOCK
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}

// ---------------------------------------------------------------------------
// Tool summaries / chips (pure)
// ---------------------------------------------------------------------------

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let mut commands = 0usize;
    let mut edited: Vec<&str> = Vec::new();
    let mut reads = 0usize;
    let mut searches = 0usize;
    let mut fetches = 0usize;
    let mut todos = 0usize;
    let mut other = 0usize;
    let mut failed = 0usize;
    for t in tools {
        if t.is_error {
            failed += 1;
        }
        match &t.call {
            ToolCall::Exec { .. } => commands += 1,
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                if !edited.contains(&path.as_str()) {
                    edited.push(path);
                }
            }
            ToolCall::ApplyPatch { path } => {
                let p = path.as_deref().unwrap_or("patch");
                if !edited.contains(&p) {
                    edited.push(p);
                }
            }
            ToolCall::ReadFile { .. } => reads += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } | ToolCall::WebSearch { .. } => {
                searches += 1
            }
            ToolCall::WebFetch { .. } => fetches += 1,
            ToolCall::Todo { .. } => todos += 1,
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => other += 1,
        }
    }
    let mut segments: Vec<String> = Vec::new();
    if commands > 0 {
        segments.push(format!("ran {}", plural(commands, "command", "commands")));
    }
    if !edited.is_empty() {
        segments.push(format!("edited {}", plural(edited.len(), "file", "files")));
    }
    if reads > 0 {
        segments.push(format!("read {}", plural(reads, "file", "files")));
    }
    if searches > 0 {
        segments.push(format!("searched {}", plural(searches, "time", "times")));
    }
    if fetches > 0 {
        segments.push(format!("fetched {}", plural(fetches, "page", "pages")));
    }
    if todos > 0 {
        segments.push("updated todos".to_string());
    }
    if other > 0 {
        segments.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if segments.is_empty() {
        segments.push(plural(tools.len(), "tool", "tools"));
    }
    if failed > 0 {
        segments.push(format!("{failed} failed"));
    }
    let mut summary = segments.join(" · ");
    // Capitalize the first segment only (comet's style).
    if let Some(first) = summary.get(0..1) {
        let upper = first.to_uppercase();
        summary.replace_range(0..1, &upper);
    }
    summary
}

/// Per-kind chip label + one-line detail.
pub fn tool_chip_content(call: &ToolCall) -> (&'static str, String) {
    // Labels match comet tool-chip.tsx `describeTool` exactly.
    match call {
        ToolCall::Exec { command } => ("Run", command.clone()),
        ToolCall::ReadFile { path } => ("Read", path.clone()),
        ToolCall::WriteFile { path, .. } => ("Write", path.clone()),
        ToolCall::EditFile { path, .. } => ("Edit", path.clone()),
        ToolCall::ApplyPatch { path } => {
            ("Patch", path.clone().unwrap_or_else(|| "workspace".into()))
        }
        ToolCall::Search { pattern, path } => (
            "Search",
            match path {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.clone(),
            },
        ),
        ToolCall::Glob { pattern } => ("Glob", pattern.clone()),
        ToolCall::WebFetch { url, .. } => ("Fetch", url.clone()),
        ToolCall::WebSearch { query } => ("Web", query.clone()),
        ToolCall::Todo { items } => {
            let done = items.iter().filter(|i| i.done).count();
            ("Todo", format!("{done}/{} done", items.len()))
        }
        ToolCall::Mcp { server, tool, .. } => ("MCP", format!("{server} · {tool}")),
        ToolCall::Unknown { name, .. } => ("Tool", name.clone()),
    }
}

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

// ---------------------------------------------------------------------------
// Working indicator flavour (pure; rendered by the shell strip)
// ---------------------------------------------------------------------------

/// Rotating flavour vocabulary (20 words / 7s, seeded per chat).
pub const FLAVOUR_WORDS: [&str; 20] = [
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The flavour word for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

async fn yield_now() {
    let mut yielded = false;
    futures::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

struct HighlightEntry {
    code_len: usize,
    lines: Option<Arc<Vec<Vec<Token>>>>,
    _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
}

impl HighlightStore {
    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<Vec<Vec<Token>>>> {
        let key = (row_id.clone(), block_ix);
        if let Some(entry) = self.entries.get(&key)
            && entry.code_len == code.len()
        {
            return entry.lines.clone();
        }
        // Keep stale lines visible while the fresh parse runs (paint-only, so a
        // briefly stale color is harmless; lengths shift at most on the tail).
        let stale = self.entries.get(&key).and_then(|e| e.lines.clone());
        let code = code.to_string();
        let code_len = code.len();
        let task = cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let mut carry = LineCarry::None;
                    let mut out = Vec::new();
                    for (ix, line) in code.split('\n').enumerate() {
                        let (tokens, next) = tokenize_line(lang, line, carry);
                        carry = next;
                        out.push(tokens);
                        if ix % 128 == 127 {
                            yield_now().await;
                        }
                    }
                    out
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(entry) = transcript.highlights.entries.get_mut(&key)
                    && entry.code_len == code_len
                {
                    entry.lines = Some(Arc::new(lines));
                    cx.notify();
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                code_len,
                lines: stale.clone(),
                _task: Some(task),
            },
        );
        stale
    }
}

// ---------------------------------------------------------------------------
// Transcript entity
// ---------------------------------------------------------------------------

struct CachedRows {
    fingerprint: u64,
    rows: Vec<Row>,
}

#[derive(Default, Clone, Copy)]
struct FoldState {
    /// User pin (click); `None` follows the auto-open rule.
    open: Option<bool>,
    /// Bumped per toggle — keys the 200ms height tween.
    epoch: usize,
    /// Height at the moment of the toggle (the tween's start). The destination
    /// is always the *current* target height, so content growth after a toggle
    /// snaps instead of replaying a stale tween.
    from: f32,
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<String>,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    highlights: HighlightStore,
    show_jump_button: bool,
    scroll_anim: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    _observe: Subscription,
}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW_PX));
        list.set_follow_mode(FollowMode::Tail);
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            chat_id: None,
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            veils: HashMap::new(),
            highlights: HighlightStore::default(),
            show_jump_button: false,
            scroll_anim: None,
            rail_enabled: true,
            rail_hover: None,
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    // ---- rail plumbing (rendering lives in crate::rail) ----

    /// Shell-driven width gate: the rail hides below 48rem of container width.
    pub fn set_rail_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.rail_enabled != enabled {
            self.rail_enabled = enabled;
            cx.notify();
        }
    }

    pub(crate) fn rail_enabled(&self) -> bool {
        self.rail_enabled
    }

    pub(crate) fn rail_hover(&self) -> Option<usize> {
        self.rail_hover
    }

    pub(crate) fn set_rail_hover(&mut self, hover: Option<usize>) {
        self.rail_hover = hover;
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn list_state(&self) -> &ListState {
        &self.list
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Replace the transcript's scroll animation task (rail click / jump).
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.list.set_follow_mode(FollowMode::Normal);
        self.scroll_anim = Some(task);
    }

    fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    fn handle_scroll(&mut self, event: &ListScrollEvent, cx: &mut Context<Self>) {
        let distance = self.distance_from_bottom();
        // Re-engage the pin when a user scroll returns within the stick band.
        // (The pin is broken only by user input — the list stops following on
        // wheel/drag up in its own input path, never from content growth.)
        if !event.is_following_tail && distance <= STICK_THRESHOLD_PX {
            self.list.set_follow_mode(FollowMode::Tail);
        }
        let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !self.list.is_following_tail();
        if show != self.show_jump_button {
            self.show_jump_button = show;
            cx.notify();
        }
    }

    /// Own-send re-engage: smooth-scroll to the end, then pin.
    pub fn on_own_send(&mut self, cx: &mut Context<Self>) {
        if self.list.is_following_tail() {
            self.list.scroll_to_end();
            cx.notify();
            return;
        }
        self.animate_scroll_to_bottom(cx);
    }

    fn animate_scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroll_anim = Some(cx.spawn(async move |this, cx| {
            for _ in 0..60 {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let done = this.update(cx, |t, cx| {
                    let remaining = t.distance_from_bottom();
                    if remaining <= 2.0 {
                        t.list.set_follow_mode(FollowMode::Tail);
                        t.list.scroll_to_end();
                        t.show_jump_button = false;
                        cx.notify();
                        true
                    } else {
                        // Exponential ease-out step toward the bottom.
                        let step = (remaining * 0.28).max(24.0).min(remaining);
                        t.list.scroll_by(px(step));
                        cx.notify();
                        false
                    }
                });
                match done {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, entries, echoes) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone(),
                s.transcript.clone(),
                s.pending_echoes().to_vec(),
            )
        };

        if selected != self.chat_id {
            self.chat_id = selected;
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.veils.clear();
            self.highlights.entries.clear();
            self.list.reset(0);
            self.list.set_follow_mode(FollowMode::Tail);
            self.show_jump_button = false;
        }

        let mut new_rows: Vec<Row> = Vec::new();
        for entry in &entries {
            new_rows.extend(self.rows_for(entry, false));
        }
        for echo in &echoes {
            new_rows.extend(self.rows_for(echo, true));
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→split handoff (any mid-fade chunk snaps to full, matching the
        // flip's row splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;
                return;
            }
            Some((old_range, count)) => {
                self.list.splice(old_range, count);
            }
        }
        self.rows = new_rows;
        if self.list.is_following_tail() {
            self.list.scroll_to_end();
        }
        cx.notify();
    }

    /// Cached row build for one entry (streaming entries bypass the cache).
    fn rows_for(&mut self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, pending);
        if !streaming
            && let Some(cached) = self.row_cache.get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            if streaming {
                let parser = live_parsers.entry(key.to_string()).or_default();
                parser.set_text(text);
                Arc::new(parser.tree().clone())
            } else {
                if let Some((len, tree)) = tree_cache.get(key)
                    && *len == text.len()
                {
                    return tree.clone();
                }
                // On the live→complete flip reuse the live parser's tree when
                // the sources match — the split rows then share the exact tree
                // the unsplit row painted, guaranteeing a flicker-free handoff.
                let tree = match live_parsers.remove(key) {
                    Some(parser) if parser.source() == text => Arc::new(parser.tree().clone()),
                    _ => Arc::new(parse_full(text)),
                };
                tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
                tree.clone()
            }
        };
        let rows = rows_for_entry(entry, pending, &mut parse);

        if !streaming {
            self.row_cache.insert(
                entry.id.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
        }
        rows
    }

    fn toggle_fold(&mut self, row_id: SharedString, tool_count: usize, auto_open: bool) {
        let entry = self.folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(auto_open);
        entry.from = if currently_open {
            chips_height(tool_count)
        } else {
            0.0
        };
        entry.open = Some(!currently_open);
        entry.epoch += 1;
    }

    // ---- rendering ----

    fn render_row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let top_gap = if ix == 0 {
            GAP_TURN + 10.0
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
        };
        let bottom_pad = if ix + 1 == self.rows.len() { 24.0 } else { 0.0 };

        let inner: AnyElement = match &row.kind {
            RowKind::User { text, pending } => {
                let bubble = div().flex().justify_end().child(
                    div()
                        .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                        .bg(theme.surface_raised)
                        .rounded(px(Theme::BUBBLE_RADIUS))
                        .px(px(16.0))
                        .py(px(10.0))
                        .text_size(px(14.0))
                        .line_height(px(22.0))
                        .text_color(theme.text)
                        .when(*pending, |el| el.opacity(0.65))
                        .child(text.clone()),
                );
                bubble.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let opts = RenderOptions::settled(row.id.clone());
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                render::render_block(
                    &top.block,
                    *block_ix,
                    &opts,
                    &theme,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|v| v.as_slice()),
                )
            }
            RowKind::LiveMarkdown { tree } => {
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                let veil = (!motion::reduced_motion(cx))
                    .then(|| self.veils.entry(row.id.clone()).or_default().clone());
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    now: Instant::now(),
                };
                let highlight = self.code_highlight_for(&row.id, tree, None, cx);
                let el = render::render_tree(tree, &opts, &theme, &|ix| {
                    highlight.get(&ix).and_then(|o| o.clone())
                });
                // Drive the veil clock: while any chunk is still dissolving,
                // repaint next frame (self-limiting — one callback per frame).
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    let id = cx.entity_id();
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
                el
            }
            RowKind::ToolGroup { tools, auto_open } => {
                self.render_tool_group(&row.id, tools, *auto_open, &theme, cx)
            }
            RowKind::InputChip {
                questions,
                resolved,
            } => {
                let (label, color) = if *resolved {
                    ("Input provided", theme.text_muted)
                } else {
                    ("Awaiting input", theme.warning)
                };
                chip_row(
                    format!("{label} · {}", plural(*questions, "question", "questions")),
                    color,
                    &theme,
                )
            }
            RowKind::ErrorChip { message } => chip_row(message.to_string(), theme.danger, &theme),
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (comet `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .child(inner),
            )
            .into_any_element()
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<Vec<Vec<Token>>>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language.as_deref().and_then(lang_for_tag)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        auto_open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(auto_open);
        let target = if open { chips_height(tools.len()) } else { 0.0 };
        let summary = tool_group_summary(tools);
        let any_error = tools.iter().any(|t| t.is_error);

        let toggle_id = row_id.clone();
        let tool_count = tools.len();
        // Header (comet tool-group.tsx): a small chevron tile centered over the
        // chips' guide rail, then the quiet 12px summary.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .h(px(26.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(if any_error {
                theme.danger
            } else {
                theme.text_muted
            })
            .hover(|s| s.text_color(Theme::dark().text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), tool_count, auto_open);
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .bg(crate::theme::white_alpha(0.06))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(tools.iter().map(|tool| tool_chip(tool, theme)));

        // Fold body: 200ms committed-height tween on a USER toggle only.
        // Auto-open (streaming) and content growth never tween — the closure
        // lerps toward the *current* target, so tools arriving mid- or
        // post-tween snap the destination instead of replaying a stale height
        // (only `open` toggles animate — composes with the stick spring).
        let body: AnyElement = if fold.epoch > 0 {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(chips)
                .with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .h(px(target))
                .child(chips)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

fn chip_row(text: String, color: gpui::Hsla, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .child(
            div()
                .max_w_full()
                .rounded(px(Theme::CONTROL_RADIUS))
                .border_1()
                .border_color(theme.border)
                .px(px(10.0))
                .py(px(4.0))
                .text_size(px(12.0))
                .text_color(color)
                .truncate()
                .child(SharedString::from(text)),
        )
        .into_any_element()
}

/// A small glyph standing in for the tool's icon (comet uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (comet tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// One tool chip row: a guide rail on the left (continuous across stacked
/// chips — the rail spans the row's full height) threading the chips to their
/// group toggle, then the chip card (comet tool-chip.tsx).
fn tool_chip(tool: &ToolItem, theme: &Theme) -> AnyElement {
    let (label, detail) = tool_chip_content(&tool.call);
    let tint = if tool.is_error {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        // Guide rail: hairline centered under the header's chevron tile.
        .child(
            div()
                .ml(px(12.0))
                .h_full()
                .w(px(1.0))
                .flex_none()
                .bg(crate::theme::white_alpha(0.08)),
        )
        .child(
            div()
                .ml(px(12.0))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::white_alpha(0.07))
                .bg(crate::theme::white_alpha(0.03))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    // Icon tile (`size-[18px] rounded-[5px] bg-white/[0.08]`,
                    // icon size-3).
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .bg(crate::theme::white_alpha(0.08))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(tool_icon_path(&tool.call))
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(tint)
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if tool.is_error {
                            theme.danger
                        } else {
                            theme.text.opacity(0.85)
                        })
                        .child(SharedString::from(detail)),
                ),
        )
        .into_any_element()
}

fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error, resolved, ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
    }
    fnv1a(&acc)
}

impl Render for Transcript {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let (raised, border, text) = (theme.surface_raised, theme.border, theme.text);
        let jump = self.show_jump_button;
        let rail = self.render_rail(cx);
        div()
            .relative()
            .size_full()
            .min_h_0()
            .child(
                list(self.list.clone(), cx.processor(Self::render_row))
                    .size_full()
                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
            )
            .child(rail)
            .when(jump, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(16.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(motion::fade_quick(
                            "jump-to-bottom",
                            div()
                                .id("jump-to-bottom-btn")
                                .size(px(32.0))
                                .rounded_full()
                                .bg(raised)
                                .border_1()
                                .border_color(border)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(14.0))
                                .text_color(text)
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.animate_scroll_to_bottom(cx);
                                }))
                                .child(SharedString::from("↓")),
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_doc::MessagePart;

    fn parse(_: &str, text: &str) -> Arc<BlockTree> {
        Arc::new(parse_full(text))
    }

    fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: Some(status),
            continuation_of: None,
        }
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn tool_part(id: &str, command: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
            is_error: false,
            resolved: true,
        }
    }

    const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

    #[test]
    fn live_entry_stays_unsplit_and_splits_on_complete_with_id_continuity() {
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        assert_eq!(live_rows.len(), 1, "live text stays one row");
        assert!(matches!(live_rows[0].kind, RowKind::LiveMarkdown { .. }));
        assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");

        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        assert_eq!(done_rows.len(), 3, "three top-level blocks");
        // First split block reuses the live row id — no flicker on handoff.
        assert_eq!(done_rows[0].id, live_rows[0].id);
        assert_eq!(done_rows[1].id.as_ref(), "m1#t0.1");
        assert!(matches!(
            done_rows[0].kind,
            RowKind::Markdown { block_ix: 0, .. }
        ));
        // The flip changes the version even at identical text, forcing a splice.
        assert_ne!(done_rows[0].version, live_rows[0].version);
    }

    #[test]
    fn split_sibling_gaps_match_live_internal_spacing() {
        // The live row spaces its internal blocks by MD_BLOCK_GAP; after the
        // live→split handoff the same boundaries are inter-row gaps. They must
        // be identical or the whole message jumps at completion.
        let done = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                text_part("t0", MD),
                tool_part("a", "ls"),
                text_part("t1", "tail para"),
            ],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        // Rows: t0.0, t0.1, t0.2 (three MD blocks), g0, t1.0.
        assert_eq!(rows.len(), 5);
        // Sibling markdown blocks from the same part: md block gap.
        assert_eq!(
            top_gap_for(Some(&rows[0]), &rows[1]),
            render::MD_BLOCK_GAP
        );
        assert_eq!(
            top_gap_for(Some(&rows[1]), &rows[2]),
            render::MD_BLOCK_GAP
        );
        // Markdown → tool group and tool group → next part: block gap.
        assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), GAP_BLOCK);
        assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), GAP_BLOCK);
        // Turn starts get the turn gap regardless.
        assert_eq!(top_gap_for(None, &rows[0]), GAP_TURN);
    }

    #[test]
    fn consecutive_tools_fold_into_groups_between_text() {
        let entry = assistant(
            "m2",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                text_part("t1", "after"),
                tool_part("c", "make"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["m2#t0.0", "m2#g0", "m2#t1.0", "m2#g1"]);
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!("group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(rows[0].turn_start && !rows[1].turn_start);
    }

    #[test]
    fn trailing_group_auto_opens_only_while_streaming() {
        let parts = vec![text_part("t0", "hi"), tool_part("a", "ls")];
        let streaming = assistant("m3", MessageStatus::Streaming, parts.clone());
        let rows = rows_for_entry(&streaming, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(auto_open, "trailing group opens while streaming");

        let complete = assistant("m3", MessageStatus::Complete, parts);
        let rows = rows_for_entry(&complete, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(!auto_open);

        // A non-trailing group never auto-opens.
        let mid = assistant(
            "m4",
            MessageStatus::Streaming,
            vec![tool_part("a", "ls"), text_part("t0", "hi")],
        );
        let rows = rows_for_entry(&mid, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[0].kind else {
            panic!()
        };
        assert!(!auto_open);
    }

    #[test]
    fn user_rows_and_echo_versions() {
        let mut entry = assistant("u1", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", "hello")];
        let confirmed = rows_for_entry(&entry, false, &mut parse);
        let echoed = rows_for_entry(&entry, true, &mut parse);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].id, echoed[0].id);
        // Pending → confirmed changes the version so the row re-renders.
        assert_ne!(confirmed[0].version, echoed[0].version);
        assert!(matches!(
            &echoed[0].kind,
            RowKind::User { pending: true, .. }
        ));
    }

    #[test]
    fn diff_rows_appends_and_middle_edits() {
        let entry1 = assistant("m1", MessageStatus::Complete, vec![text_part("t0", "one")]);
        let entry2 = assistant("m2", MessageStatus::Complete, vec![text_part("t0", "two")]);
        let r1 = rows_for_entry(&entry1, false, &mut parse);
        let mut both = r1.clone();
        both.extend(rows_for_entry(&entry2, false, &mut parse));

        // Identical → None.
        assert!(diff_rows(&r1, &r1.clone()).is_none());
        // Append → splice at the tail.
        assert_eq!(diff_rows(&r1, &both), Some((1..1, 1)));
        // Removal from the end.
        assert_eq!(diff_rows(&both, &r1), Some((1..2, 0)));

        // Middle content change: only the changed row splices.
        let entry1b = assistant(
            "m1",
            MessageStatus::Complete,
            vec![text_part("t0", "one more")],
        );
        let mut both_b = rows_for_entry(&entry1b, false, &mut parse);
        both_b.extend(rows_for_entry(&entry2, false, &mut parse));
        assert_eq!(diff_rows(&both, &both_b), Some((0..1, 1)));

        // Full reset when everything shifts.
        let r2 = rows_for_entry(&entry2, false, &mut parse);
        assert_eq!(diff_rows(&r1, &r2), Some((0..1, 1)));
    }

    #[test]
    fn diff_handles_live_to_split_growth() {
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        // One live row becomes three split rows in a single splice at 0.
        assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..1, 3)));
    }

    #[test]
    fn tool_group_summaries() {
        let exec = |c: &str| ToolItem {
            call: ToolCall::Exec { command: c.into() },
            is_error: false,
            resolved: true,
        };
        let edit = |p: &str| ToolItem {
            call: ToolCall::EditFile {
                path: p.into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
        };
        let tools = vec![
            exec("ls"),
            exec("pwd"),
            exec("make"),
            edit("a.rs"),
            edit("b.rs"),
        ];
        assert_eq!(
            tool_group_summary(&tools),
            "Ran 3 commands · edited 2 files"
        );
        // Distinct-path dedupe: editing one file twice counts once.
        let tools = vec![edit("a.rs"), edit("a.rs")];
        assert_eq!(tool_group_summary(&tools), "Edited 1 file");
        // Failures append.
        let mut failing = exec("boom");
        failing.is_error = true;
        assert_eq!(tool_group_summary(&[failing]), "Ran 1 command · 1 failed");
        // Reads / searches / misc.
        let tools = vec![
            ToolItem {
                call: ToolCall::ReadFile { path: "x".into() },
                is_error: false,
                resolved: true,
            },
            ToolItem {
                call: ToolCall::Glob {
                    pattern: "*.rs".into(),
                },
                is_error: false,
                resolved: true,
            },
            ToolItem {
                call: ToolCall::WebSearch { query: "q".into() },
                is_error: false,
                resolved: true,
            },
        ];
        assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
    }

    #[test]
    fn tool_chip_labels_per_kind() {
        assert_eq!(
            tool_chip_content(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("Run", "cargo test".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }),
            ("Search", "foo in src".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::ApplyPatch { path: None }),
            ("Patch", "workspace".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
        let todo = ToolCall::Todo {
            items: vec![
                comet_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                comet_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
    }

    #[test]
    fn chips_height_is_analytic() {
        assert_eq!(chips_height(0), 0.0);
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        assert_eq!(
            chips_height(3),
            CHIPS_TOP_PAD + 3.0 * CHIP_HEIGHT + 2.0 * CHIP_GAP
        );
    }

    #[test]
    fn flavour_words_rotate_every_seven_seconds() {
        let seed = flavour_seed("chat-1");
        assert_eq!(flavour_word(seed, 0), flavour_word(seed, 6));
        assert_ne!(flavour_word(seed, 0), flavour_word(seed, 7));
        // Deterministic per chat; different chats usually differ in phase.
        assert_eq!(flavour_word(seed, 3), flavour_word(seed, 3));
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(92), "1m 32s");
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn empty_text_parts_produce_no_rows() {
        let entry = assistant(
            "m9",
            MessageStatus::Streaming,
            vec![text_part("t0", ""), text_part("t1", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }
}
