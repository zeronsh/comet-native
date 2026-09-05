//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups
//!   (agent/spawn chips split out so they never collapse) and input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; the first send in an empty chat anchors the prompt at
//! the viewport top and hands off to the same glide when the reply overflows.
//! Wheel/touch releases that anchor immediately, including when background
//! streaming has advanced beyond the last measured frame.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, Bounds, ClipboardItem, Context, Entity, ListAlignment, ListOffset,
    ListScrollEvent, ListState, MouseButton, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels,
    Point, ScrollHandle, SharedString, StyledImage as _, StyledText, Subscription, Task, TextRun,
    Window, canvas, div, img, list, prelude::*, px, quad,
};

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use zeron_proto::ToolCall;

use crate::markdown::parser::{
    Block, BlockTree, IncrementalParser, InlineRun, InlineStyle, parse_full,
};
use crate::markdown::render::{self, RenderCache, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;
use zeron_syntax::LanguageId as Lang;

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
pub const OVERDRAW_PX: f32 = 320.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Bound session-local viewport memory independently of total chat history.
const MAX_SAVED_VIEWPORTS: usize = 256;
/// Text-selection edge scrolling runs only during a drag. A 24 ms cadence is
/// smooth enough to track text while avoiding a permanent animation-frame loop
/// on low-end devices.
const SELECTION_SCROLL_TICK_MS: u64 = 24;
const SELECTION_SCROLL_EDGE_PX: f32 = 36.0;
const SELECTION_SCROLL_MAX_STEP_PX: f32 = 24.0;
/// Transcript column max width (zeron 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// Activity row height / gap — analytic, so fold heights need no measurement.
/// Ordinary tools place their icon on the rail; subagents retain a 30px card.
/// Rows stack without a gap so the rail continues alongside expanded output.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
/// Inner height of the chip header: [`CHIP_CARD_HEIGHT`] is the card's
/// border-box (explicit `h` in gpui includes the 1px border), so a 30px
/// header inside a 30px bordered card clips 2px off the bottom and every
/// glyph/icon reads high (user report).
const CHIP_HEADER_HEIGHT: f32 = CHIP_CARD_HEIGHT - 2.0;
/// Shared columns keep the group summary, tool labels, and expanded text aligned.
const ACTIVITY_GUTTER_WIDTH: f32 = 26.0;
const ACTIVITY_TEXT_GAP: f32 = 4.0;
const TOOL_TEXT_SIZE: f32 = 12.0;

/// Signed list scroll step for a pointer near a viewport edge.
///
/// GPUI list offsets increase toward the document bottom. The quadratic ramp
/// keeps entry into the edge zone gentle and reaches full speed at the edge.
fn selection_scroll_step(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
    let height = f32::from(bounds.size.height);
    if height <= 0.0 {
        return 0.0;
    }
    let edge = SELECTION_SCROLL_EDGE_PX.min(height / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let y = f32::from(position.y);
    let top = f32::from(bounds.top());
    let bottom = f32::from(bounds.bottom());
    let scaled = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        SELECTION_SCROLL_MAX_STEP_PX * t * t
    };
    if y < top + edge {
        -scaled(top + edge - y)
    } else if y > bottom - edge {
        scaled(y - (bottom - edge))
    } else {
        0.0
    }
}
const CHIPS_TOP_PAD: f32 = 2.0;
/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// A user prompt renders at most this many wrapped lines until expanded. A
/// pasted log or file drops into the transcript as one endless slab otherwise
/// (user report) — past the cap the bubble clips and grows a chevron.
pub const USER_COLLAPSED_LINES: usize = 5;
/// The user bubble's line box.
pub const USER_LINE_HEIGHT: f32 = 22.0;
/// Conservative first-frame soft-wrap proxy for the fixed-width long-prompt
/// bubble. The final decision uses the wrapped `StyledText` layout, but this
/// fallback lets clearly long prompts render their affordance immediately
/// before that first layout has completed.
pub const USER_COLLAPSE_CHARS: usize = 400;
/// Vertical separation before the plain expand/collapse link.
const USER_TOGGLE_GAP: f32 = 8.0;
/// User-bubble attachment thumbnails (user-attachments.tsx): 112×80 thumbs in
/// a FIXED-height strip (load-state flips never shift the virtualizer).
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;

// ---------------------------------------------------------------------------
// Stick-to-bottom spring (mugen §1e — same constants as its DEFAULT_SPRING,
// which follows the shape of stackblitz/use-stick-to-bottom)
// ---------------------------------------------------------------------------

/// Retains velocity frame-to-frame (higher = more glide).
pub const SPRING_DAMPING: f32 = 0.7;
/// Pull toward the target (higher = snappier).
pub const SPRING_STIFFNESS: f32 = 0.05;
/// Inertia (higher = slower to start/stop).
pub const SPRING_MASS: f32 = 1.25;
/// Reference frame for the fixed-timestep integration (60fps).
pub const SPRING_FRAME_MS: f32 = 1000.0 / 60.0;
/// Cap on simulated frames per tick — a hitch catches up instead of teleporting.
pub const SPRING_MAX_CATCHUP_FRAMES: f32 = 8.0;
/// EMA rate for the feed-forward target-growth estimate.
pub const SPRING_GROWTH_EMA: f32 = 0.12;
/// While streaming, chase up to this many px above the true bottom (keeps the
/// growing tail visible instead of hugging a moving edge).
pub const SPRING_CHASE_MAX_LEAD: f32 = 32.0;
/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;

/// A live stream already resting at the end should keep that end anchored as
/// its measured height grows. This is deliberately narrower than `pinned`:
/// users gliding back toward the bottom keep the normal spring behavior.
fn should_anchor_live_stream(pinned: bool, distance_from_bottom: f32, streaming: bool) -> bool {
    pinned && streaming && distance_from_bottom <= AT_BOTTOM_PX
}

/// Retain the spring's state this long after landing, so a streaming pause
/// resumes at cruise. Retaining state does not require drawing idle frames.
pub const SPRING_SETTLE_GRACE_MS: u64 = 500;
/// Teleport when farther than this many viewports from the end; glide the rest.
pub const GLIDE_MAX_VIEWPORTS: f32 = 2.5;
/// A freshly-sent prompt rests this far below the transcript viewport's top.
/// The titlebar overlays the full-height list, so its height is part of the
/// inset; the extra 10px matches the first row's breathing room.
pub(crate) const OWN_SEND_TOP_INSET_PX: f32 = Theme::TITLEBAR_HEIGHT + 10.0;
/// Epsilon of extra height under the reservation. The runway ends AT the
/// app's bottom — this is not scroll room (24px of it read as a janky
/// overshoot-and-fight zone, user report) — it exists only to keep the held
/// layout out of gpui's shorter-than-viewport regime, where a bottom-aligned
/// list reports no item bounds (sizing goes blind) and position becomes a
/// function of content height instead of the hold. Two pixels of travel is
/// below perception.
const OWN_SEND_SCROLL_SLACK_PX: f32 = 2.0;
/// Per-60fps-frame fraction of the remaining entry glide retained (~90%
/// covered in ~230ms, ease-out).
const OWN_SEND_GLIDE_RETAIN: f32 = 0.85;
/// The entry glide snaps to the absolute hold within this error.
const OWN_SEND_GLIDE_SNAP_PX: f32 = 1.0;

/// A bounds-free guard for gliding through rows whose heights are still
/// being measured. The provisional reservation is never a scroll target.
fn own_turn_glide_crossed(offset: ListOffset, anchor_ix: usize, inset: f32) -> bool {
    offset.item_ix > anchor_ix
        || (offset.item_ix == anchor_ix && f32::from(offset.offset_in_item) > -inset)
}

/// Pure stick-to-bottom spring stepper — the mugen `tick()` integration:
/// velocity relaxes toward `(damping·v + stiffness·diff)/mass` per 60fps
/// sub-frame, position advances by `v + target_vel` where `target_vel` is a
/// feed-forward EMA of target growth px/frame, and the chase point sits up to
/// [`SPRING_CHASE_MAX_LEAD`] px above the true bottom proportional to growth.
#[derive(Debug, Clone, Copy)]
pub struct StickSpring {
    /// Spring velocity, px per 60fps frame.
    velocity: f32,
    /// Feed-forward: smoothed target growth, px per 60fps frame.
    target_vel: f32,
    /// Target observed at the previous tick (`None` = fresh/parked).
    last_target: Option<f32>,
}

impl Default for StickSpring {
    fn default() -> Self {
        Self::new()
    }
}

impl StickSpring {
    pub fn new() -> Self {
        Self {
            velocity: 0.0,
            target_vel: 0.0,
            last_target: None,
        }
    }

    /// Park the spring (drops all state; the next tick starts cold).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Residual motion below mugen's settle thresholds (`v < .05 && targetVel
    /// < .05`)?
    pub fn is_idle(&self) -> bool {
        self.velocity < 0.05 && self.target_vel < 0.05
    }

    fn needs_frame(distance: f32) -> bool {
        // The spring is clamped to the target. Residual velocity cannot move
        // a viewport already there; virtual-list height estimates can keep
        // that velocity nonzero indefinitely even after a turn completes.
        distance > 0.5
    }

    #[cfg(test)]
    pub(crate) fn target_vel(&self) -> f32 {
        self.target_vel
    }

    /// Advance one tick. `pos`/`target` are scroll offsets in px (larger =
    /// closer to the bottom); `frames` is elapsed time in 60fps frames
    /// (clamped by the caller to [`SPRING_MAX_CATCHUP_FRAMES`]). Returns the
    /// new position: never overshoots `target`, monotone while approaching,
    /// and snaps exactly once within 0.5px.
    pub fn step(&mut self, mut pos: f32, target: f32, mut frames: f32) -> f32 {
        let grew = self.last_target.map_or(0.0, |last| target - last);
        self.last_target = Some(target);
        if grew < -1.0 {
            // Target shrank (row collapse/removal) — growth estimate is stale.
            self.target_vel = 0.0;
        } else {
            let observed = grew.max(0.0) / frames.max(0.25);
            self.target_vel += SPRING_GROWTH_EMA * (observed - self.target_vel);
        }
        let chase = target - (self.target_vel * 9.0).min(SPRING_CHASE_MAX_LEAD);
        let mut v = self.velocity;
        while frames > 0.0 {
            let h = frames.min(1.0);
            frames -= h;
            let diff = (chase - pos).max(0.0);
            v += h * ((SPRING_DAMPING * v + SPRING_STIFFNESS * diff) / SPRING_MASS - v);
            pos = (pos + (v + self.target_vel) * h).min(target);
        }
        self.velocity = v;
        if target - pos <= 0.5 { target } else { pos }
    }
}

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
    /// Expandable detail: a code-block of output lines, or a real diff
    /// section rendered by the changes pane's component (ACP harnesses).
    /// Precomputed here because rows are cached by fingerprint — diffing and
    /// tokenizing per paint would run on every scroll frame.
    pub detail: Option<Arc<ToolDetail>>,
    /// Expandable full-invocation block: the complete tool call (whole
    /// command / pattern / URL / input JSON) that the chip header collapses
    /// to one truncated line. Rendered above `detail` in the open card.
    /// Precomputed for the same reason as `detail`.
    pub invocation: Option<Arc<ToolDetail>>,
    /// Sidecar key of the full output (chat2-sync A3) — the doc carries only
    /// a one-line summary; expanding offers a lazy "Show full output" fetch.
    pub output_ref: Option<SharedString>,
    /// Full-output size, for the affordance label ("Show full output (12 KB)").
    pub output_bytes: Option<u64>,
    /// Sidecar key of the full diff (doc carries only per-file stats).
    pub diff_ref: Option<SharedString>,
    /// The spawned SUBAGENT's doc id — the chip IS the index (there is no
    /// listing endpoint); with it the chip offers "Open subagent".
    pub subagent_ref: Option<SharedString>,
    /// Subagent lifecycle, distinct from `resolved` (eager-done: the spawn
    /// tool's own result lands while the subagent still runs).
    pub subagent_status: Option<SubagentStatus>,
    /// One-line live tail — LEGACY docs only (new runs stopped folding it;
    /// per-delta header rewrites read as noise). Never rendered; still
    /// fingerprinted so an old doc's chips re-splice correctly.
    pub subagent_tail: Option<SharedString>,
    /// A REASONING part riding the tool group as a chip (user request: the
    /// thought process belongs inside the combined "Ran N commands"
    /// accordion, opening/closing with the same tween). Synthesized in
    /// [`rows_for_entry`] — never comes from a doc tool part. The thought
    /// text is the `detail`; `resolved == false` means it is still
    /// streaming (the chip then defaults open).
    pub is_thought: bool,
}

/// Subagent spawn chips — [`ToolCall::is_subagent_spawn`], the shared genus
/// every driver decodes its spawn tool into. These stay out of the
/// collapsible "Called N tools" wrap so a running subagent is visible
/// without opening the fold.
fn is_agent_call(call: &ToolCall) -> bool {
    call.is_subagent_spawn()
}

/// The chip's GENUS is the call itself, never the ref: docs written before
/// the claude-driver fix carry stray `subagent_ref`s on ordinary Run chips
/// (a background shell's `task_notification` was mis-tagged as subagent
/// traffic), and honoring the ref alone turned those Runs into spawn chips
/// that opened empty, never-created subagent docs.
fn is_agent_tool(item: &ToolItem) -> bool {
    is_agent_call(&item.call)
}

/// A chip renders as the spawn LINK (whole-card click → subagent tab) only
/// when an agent call has actually been bound to its doc.
fn is_spawn_link(item: &ToolItem) -> bool {
    is_agent_call(&item.call) && item.subagent_ref.is_some()
}

/// Ordinary tool groups fold behind a summary header; agent/spawn chips
/// render as their own always-open row.
fn tool_group_collapses(tools: &[ToolItem]) -> bool {
    tools.iter().any(|t| !is_agent_tool(t))
}

/// Column budget for soft-wrapping thought text into detail lines. The
/// detail body is preformatted (no element wrapping), so the wrap happens
/// here — conservative enough to fit the card at typical transcript widths.
const THOUGHT_WRAP_COLS: usize = 96;

/// Flatten a thought's parsed markdown into wrapped, STYLED detail lines —
/// inline markers render as real styling (bold/italic/code/links) instead of
/// literal `**` glyphs; blocks flatten structurally (headings bold, list
/// bullets, quote bars, verbatim code lines). Every line is one fixed-height
/// row, so the detail height stays analytic (lines × [`OUTPUT_LINE_HEIGHT`])
/// and the group's fold tween keeps working without measurement.
fn thought_lines(tree: &BlockTree) -> Vec<Vec<InlineRun>> {
    let mut out: Vec<Vec<InlineRun>> = Vec::new();
    for top in &tree.blocks {
        if !out.is_empty() {
            // One blank separator row between top-level blocks (the old
            // plain-text wrap kept paragraph gaps the same way).
            out.push(Vec::new());
        }
        thought_block_lines(&top.block, 0, &mut out);
    }
    while out
        .last()
        .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
    {
        out.pop();
    }
    out
}

/// The slot-0 indent run every emitted thought line opens with (possibly
/// empty). List/quote handlers rewrite it in place to plant markers/bars, so
/// it must exist even at zero indent.
fn indent_run(indent: usize) -> Vec<InlineRun> {
    vec![InlineRun {
        text: " ".repeat(indent),
        style: InlineStyle::default(),
    }]
}

/// Append text to a line's run list, merging into the tail run when styles
/// match (keeps run counts small for the shaper).
fn push_styled(line: &mut Vec<InlineRun>, text: &str, style: &InlineStyle) {
    if text.is_empty() {
        return;
    }
    match line.last_mut() {
        Some(last) if last.style == *style => last.text.push_str(text),
        _ => line.push(InlineRun {
            text: text.to_owned(),
            style: style.clone(),
        }),
    }
}

/// Close a wrapped line: the slot-0 indent run in front (see [`indent_run`]).
fn finish_line(indent: usize, mut line: Vec<InlineRun>) -> Vec<InlineRun> {
    let mut full = indent_run(indent);
    full.append(&mut line);
    full
}

/// Word-wrap styled runs at the thought column budget. Char-counted like
/// every detail wrap — block heights must stay analytic — with words glued
/// across style boundaries (`**bold**tail` wraps as one unit), separator
/// spaces riding the preceding run, and pathological overlong tokens
/// hard-split at the budget. Hard breaks (`\n` runs) split into
/// separately-wrapped segments.
fn wrap_styled_runs(runs: &[InlineRun], indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    let budget = THOUGHT_WRAP_COLS.saturating_sub(indent).max(16);
    let mut segments: Vec<Vec<InlineRun>> = vec![Vec::new()];
    for run in runs {
        for (ix, piece) in run.text.split('\n').enumerate() {
            if ix > 0 {
                segments.push(Vec::new());
            }
            if !piece.is_empty() {
                segments.last_mut().unwrap().push(InlineRun {
                    text: piece.to_owned(),
                    style: run.style.clone(),
                });
            }
        }
    }
    for segment in segments {
        // Tokens: maximal non-whitespace piece lists, glued across run
        // boundaries so a word split by styling never wraps mid-word.
        let mut tokens: Vec<Vec<InlineRun>> = Vec::new();
        let mut in_token = false;
        for run in &segment {
            let text = run.text.as_str();
            let mut pos = 0;
            while pos < text.len() {
                let rest = &text[pos..];
                let ws = rest.chars().next().is_some_and(char::is_whitespace);
                let end = rest
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace() != ws)
                    .map_or(text.len(), |(i, _)| pos + i);
                if ws {
                    in_token = false;
                } else {
                    if !in_token {
                        tokens.push(Vec::new());
                        in_token = true;
                    }
                    push_styled(tokens.last_mut().unwrap(), &text[pos..end], &run.style);
                }
                pos = end;
            }
        }
        let mut line: Vec<InlineRun> = Vec::new();
        let mut len = 0usize;
        for token in tokens {
            let tok_len: usize = token.iter().map(|r| r.text.chars().count()).sum();
            if tok_len > budget {
                // Hard-split a pathological token at the budget.
                if len > 0 {
                    out.push(finish_line(indent, std::mem::take(&mut line)));
                    len = 0;
                }
                for piece in token {
                    let mut chars = piece.text.chars();
                    loop {
                        let chunk: String = chars.by_ref().take(budget - len).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        len += chunk.chars().count();
                        push_styled(&mut line, &chunk, &piece.style);
                        if len == budget {
                            out.push(finish_line(indent, std::mem::take(&mut line)));
                            len = 0;
                        }
                    }
                }
                continue;
            }
            if len > 0 && len + 1 + tok_len > budget {
                out.push(finish_line(indent, std::mem::take(&mut line)));
                len = 0;
            }
            if len > 0 {
                if let Some(last) = line.last_mut() {
                    last.text.push(' ');
                }
                len += 1;
            }
            for piece in token {
                push_styled(&mut line, &piece.text, &piece.style);
            }
            len += tok_len;
        }
        if len > 0 {
            out.push(finish_line(indent, line));
        }
    }
}

/// One markdown block into thought detail lines, `indent` spaces deep.
fn thought_block_lines(block: &Block, indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    match block {
        Block::Paragraph { runs } => wrap_styled_runs(runs, indent, out),
        Block::Heading { runs, .. } => {
            // Headings keep the detail's single type size — bold is the cue
            // (an 18px line box can't host display sizes).
            let bold: Vec<InlineRun> = runs
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r.style.bold = true;
                    r
                })
                .collect();
            wrap_styled_runs(&bold, indent, out);
        }
        Block::CodeBlock { code, .. } => {
            let style = InlineStyle {
                code: true,
                ..InlineStyle::default()
            };
            for line in code.lines() {
                for chunk in wrap_cols(line, THOUGHT_WRAP_COLS.saturating_sub(indent).max(16)) {
                    let mut row = indent_run(indent);
                    if !chunk.is_empty() {
                        row.push(InlineRun {
                            text: chunk.to_string(),
                            style: style.clone(),
                        });
                    }
                    out.push(row);
                }
            }
        }
        Block::List {
            ordered_start,
            items,
        } => {
            // Tight rendering: no blank rows inside a list.
            for (ix, item) in items.iter().enumerate() {
                let marker = match ordered_start {
                    Some(start) => format!("{}. ", start + ix as u64),
                    None => "• ".to_string(),
                };
                let inner = indent + marker.chars().count();
                let mark = out.len();
                for child in item {
                    thought_block_lines(child, inner, out);
                }
                if out.len() == mark {
                    // An empty item still shows its marker.
                    out.push(indent_run(inner));
                }
                // The item's first line trades its indent spaces for the
                // marker (the slot-0 run is always the indent).
                if let Some(first) = out[mark].first_mut() {
                    first.text = format!("{}{marker}", " ".repeat(indent));
                }
            }
        }
        Block::BlockQuote { children } => {
            let mark = out.len();
            for (ix, child) in children.iter().enumerate() {
                if ix > 0 {
                    out.push(Vec::new());
                }
                thought_block_lines(child, indent + 2, out);
            }
            // Trade the two quote-indent spaces for the bar on every quoted
            // line — replace, not overwrite: nested list handlers already
            // planted markers after their own deeper indent.
            for line in &mut out[mark..] {
                if let Some(first) = line.first_mut()
                    && first.text.len() >= indent + 2
                {
                    first.text.replace_range(indent..indent + 2, "│ ");
                }
            }
        }
        Block::Table { header, rows, .. } => {
            // A thought is a record, not a layout surface: cells joined with
            // a dot separator, header bold — no column machinery.
            let join = |cells: &[Vec<InlineRun>], bold: bool| -> Vec<InlineRun> {
                let mut line: Vec<InlineRun> = Vec::new();
                for (ix, cell) in cells.iter().enumerate() {
                    if ix > 0 {
                        push_styled(&mut line, " · ", &InlineStyle::default());
                    }
                    for r in cell {
                        let mut r = r.clone();
                        r.style.bold |= bold;
                        line.push(r);
                    }
                }
                line
            };
            wrap_styled_runs(&join(header, true), indent, out);
            for row in rows {
                wrap_styled_runs(&join(row, false), indent, out);
            }
        }
        Block::Rule => {
            let mut row = indent_run(indent);
            row.push(InlineRun {
                text: "———".into(),
                style: InlineStyle::default(),
            });
            out.push(row);
        }
    }
}

/// A reasoning part as a tool-group chip: "Thought process" header over the
/// thought's markdown flattened into styled detail lines (analytic height —
/// the group's fold tween needs it; see [`thought_lines`]). Capped like tool
/// outputs, with the counted tail. `live` = the part is still streaming
/// (chip defaults open).
fn thought_item(tree: &BlockTree, live: bool) -> ToolItem {
    let mut lines = thought_lines(tree);
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    if truncated_by > 0 {
        // Keep the TAIL while streaming (the fresh thinking is the signal);
        // settled thoughts keep the head like tool outputs do.
        if live {
            lines.drain(..truncated_by);
            // The cut can land on a block separator — drop the orphan blank.
            while lines
                .first()
                .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
            {
                lines.remove(0);
            }
        } else {
            lines.truncate(OUTPUT_DETAIL_MAX_LINES);
        }
    }
    ToolItem {
        call: ToolCall::Unknown {
            name: "Thought process".into(),
            input: None,
        },
        is_error: false,
        resolved: !live,
        detail: (!lines.is_empty()).then(|| {
            Arc::new(ToolDetail::Thought {
                lines,
                truncated_by,
            })
        }),
        invocation: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
        is_thought: true,
    }
}

/// A chip's expandable detail payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDetail {
    /// Command/tool output as a code block: verbatim lines (indentation
    /// intact), capped at [`OUTPUT_DETAIL_MAX_LINES`] with a counted tail.
    Output {
        lines: Vec<SharedString>,
        truncated_by: usize,
    },
    /// A thought's markdown, pre-flattened into wrapped STYLED lines — one
    /// fixed-height row each, so the height stays analytic like `Output`
    /// while inline markers render as real styling ([`thought_lines`]).
    Thought {
        lines: Vec<Vec<InlineRun>>,
        truncated_by: usize,
    },
    /// A file diff, in the changes pane's model: hunks with 3 lines of
    /// context, dual line numbers, and (for recognized languages) syntax
    /// tokens — rendered by `changes::render_file_body`.
    Diff {
        file: Arc<crate::changes::FileDiff>,
        old_text: Option<Arc<str>>,
        new_text: Option<Arc<str>>,
    },
    /// Per-file `+N −N` stat rows — what the thin doc keeps of an edit
    /// (chat2-sync A1). The full diff upgrades this to [`ToolDetail::Diff`]
    /// via the sidecar fetch.
    Stats {
        stats: Arc<Vec<zeron_doc::ToolDiffStat>>,
    },
}

/// Max verbatim output lines per chip before the counted tail row.
pub const OUTPUT_DETAIL_MAX_LINES: usize = 24;

/// Max diff lines an inline tool-diff detail renders — the detail is one
/// stacked element inside its transcript row, so it must stay bounded
/// (~600 lines ≈ 12.6k px, several screens of context before the cut).
pub const DIFF_DETAIL_MAX_LINES: usize = 600;

/// Per-line height of an output detail block (diff blocks use the changes
/// pane's own [`crate::changes::DIFF_LINE_HEIGHT`]).
pub const OUTPUT_LINE_HEIGHT: f32 = 18.0;

/// Vertical padding of an output detail body (py(6) × 2).
const OUTPUT_BODY_PAD: f32 = 12.0;

/// The hairline between an expanded chip's header row and its detail body.
const DETAIL_SEPARATOR: f32 = 1.0;

/// Build a tool part's expandable detail. A diff wins over raw output (it is
/// the more structured record of the same action); post-strip docs carry diff
/// STATS instead of inline diff text, which win the same way.
pub fn tool_detail(
    output: Option<&str>,
    diff: Option<&zeron_proto::ToolDiff>,
    diff_stats: Option<&[zeron_doc::ToolDiffStat]>,
) -> Option<ToolDetail> {
    if let Some(diff) = diff {
        let mut file = diff_to_file(diff);
        if file.hunks.is_empty() {
            return None;
        }
        // A transcript diff renders as one stacked element inside its row —
        // cap it so a whole-file rewrite (or fetched full-diff blob) can't
        // build tens of thousands of elements per frame. The changes pane
        // has no such cap; it virtualizes per line.
        crate::changes::truncate_file_lines(&mut file, DIFF_DETAIL_MAX_LINES);
        return Some(ToolDetail::Diff {
            file: Arc::new(file),
            old_text: diff.old_text.as_deref().map(Arc::from),
            new_text: Some(Arc::from(diff.new_text.as_str())),
        });
    }
    if let Some(stats) = diff_stats.filter(|s| !s.is_empty()) {
        return Some(ToolDetail::Stats {
            stats: Arc::new(stats.to_vec()),
        });
    }
    let output = output?;
    let mut lines: Vec<SharedString> = output
        .lines()
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    // Trim trailing blank output lines so the block hugs its content.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Columns at which an invocation line soft-wraps into continuation lines.
/// The wrap is char-counted, not measured — block heights must be analytic —
/// so the budget is sized to fit the narrowest useful transcript pane.
pub const CALL_WRAP_COLS: usize = 80;

/// Soft-wrap one raw line into [`CALL_WRAP_COLS`]-char chunks so a long
/// single-line command stays fully readable instead of ellipsizing.
fn wrap_cols(line: &str, cols: usize) -> Vec<SharedString> {
    if line.chars().count() <= cols {
        return vec![SharedString::from(line.to_owned())];
    }
    line.chars()
        .collect::<Vec<_>>()
        .chunks(cols)
        .map(|chunk| SharedString::from(chunk.iter().collect::<String>()))
        .collect()
}

/// Build a chip's full-invocation block — the complete tool call the header
/// truncates to one line: the whole command, pattern, or URL, todo items one
/// per line, MCP/unknown input as pretty-printed JSON. Reuses the output
/// code-block payload so rendering and height stay one implementation.
pub fn call_block(call: &ToolCall) -> Option<ToolDetail> {
    let text: String = match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::ReadFile { path } => path.clone(),
        ToolCall::WriteFile { path, content } => match content {
            Some(content) => format!("{path}\n{content}"),
            None => path.clone(),
        },
        ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_else(|| "workspace".into()),
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.clone(),
        },
        ToolCall::Glob { pattern } => pattern.clone(),
        ToolCall::WebFetch { url, prompt } => match prompt {
            Some(prompt) => format!("{url}\n{prompt}"),
            None => url.clone(),
        },
        ToolCall::WebSearch { query } => query.clone(),
        ToolCall::Todo { items } => items
            .iter()
            .map(|i| format!("{} {}", if i.done { "[x]" } else { "[ ]" }, i.text))
            .collect::<Vec<_>>()
            .join("\n"),
        ToolCall::Mcp {
            server,
            tool,
            input,
        } => match input {
            Some(input) => format!(
                "{server} · {tool}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => format!("{server} · {tool}"),
        },
        ToolCall::Unknown { name, input } => match input {
            Some(input) => format!(
                "{name}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => name.clone(),
        },
    };
    let mut lines: Vec<SharedString> = text
        .lines()
        .flat_map(|l| wrap_cols(l, CALL_WRAP_COLS))
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Reduce an inline [`zeron_proto::ToolDiff`] to the changes pane's
/// [`crate::changes::FileDiff`]: hunks grouped with 3 context lines, dual
/// 1-based line numbers, unified-diff hunk headers, and add/del counts.
pub fn diff_to_file(diff: &zeron_proto::ToolDiff) -> crate::changes::FileDiff {
    use crate::changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
    let old = diff.old_text.as_deref().unwrap_or("");
    let text_diff = similar::TextDiff::from_lines(old, &diff.new_text);
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    let mut max_line = 0u32;
    for group in text_diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_range.start + 1,
            old_range.len(),
            new_range.start + 1,
            new_range.len(),
        );
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        LineKind::Del
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Equal => LineKind::Context,
                };
                let old_no = change.old_index().map(|n| n as u32 + 1);
                let new_no = change.new_index().map(|n| n as u32 + 1);
                max_line = max_line.max(old_no.unwrap_or(0)).max(new_no.unwrap_or(0));
                lines.push(DiffLine {
                    kind,
                    old_no,
                    new_no,
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
        hunks.push(Hunk { header, lines });
    }
    FileDiff {
        path: diff.path.clone(),
        old_path: None,
        status: if diff.old_text.is_none() {
            FileStatus::Added
        } else {
            FileStatus::Modified
        },
        binary: false,
        notices: Vec::new(),
        hunks,
        additions,
        deletions,
        max_line,
    }
}

#[derive(Clone)]
pub enum RowKind {
    User {
        /// Visible prompt (attachment-ref trailer already stripped). When the
        /// prompt carries file mentions this is the *projected* display text —
        /// chip labels in place of the raw Markdown links.
        text: SharedString,
        /// File-mention chips over `text`, in display-byte terms. Computed
        /// once per entry change in [`rows_for_entry`] (rows are cached by
        /// fingerprint), never per frame. Empty for ordinary prompts.
        mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
        /// Image refs parsed out of the message text (message-attachments.ts):
        /// thumbnails load from the owning device via ReadAttachmentChunk.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Context the prompt folded in as text, lifted back out by `badges`.
        badges: Arc<Vec<crate::badges::MessageBadge>>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message. Split per block like
    /// completed rows (only the tail blocks' versions change per commit, so
    /// the settled prefix is never respliced or re-rendered); rendered with
    /// the fade veil.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        /// First question's header (chat-view.tsx `InputChip`: the resolved
        /// chip shows it; unresolved shows "Awaiting your answer…" — which
        /// stays TRUE even across a run death: the composer keeps the panel
        /// up until the user answers, and the engine delivers a dead run's
        /// answer as a resumed turn).
        header: SharedString,
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
    /// The owning message entry — hover anywhere on the entry's rows reveals
    /// its timestamp strip (zeron chat-view.tsx `group`/`group-hover`).
    pub entry_id: SharedString,
    /// Epoch-ms for the 16px hover-timestamp strip UNDER this row: set on the
    /// LAST row of a completed entry (user rows always; assistant rows only
    /// once streaming ends — "the turn isn't at a time yet", chat-view.tsx).
    pub timestamp: Option<i64>,
    /// Text copied by the entry-level hover action. Present only on the last
    /// settled row, beside the timestamp; tools and transport-only metadata
    /// are deliberately excluded.
    pub copy_text: Option<SharedString>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM" — the exact
/// `formatTimestamp` shape (utils.ts: short month, numeric day, hour,
/// 2-digit minutes, no leading zero on the hour). Pure over an explicit
/// timezone so tests don't depend on the host's local time.
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
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
        // Detail payload arriving (or growing) must re-splice the row even
        // when the resolved bit didn't change.
        match t.detail.as_deref() {
            None => acc.push(0),
            Some(ToolDetail::Output {
                lines,
                truncated_by,
            }) => {
                acc.push(1);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                let bytes: usize = lines.iter().map(|l| l.len()).sum();
                acc.extend_from_slice(&(bytes as u32).to_le_bytes());
            }
            Some(ToolDetail::Thought {
                lines,
                truncated_by,
            }) => {
                // Byte-exact plus style bits: a live mend can restyle runs
                // without changing the flattened length, and the row must
                // still re-splice.
                acc.push(4);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                for line in lines {
                    for run in line {
                        acc.extend_from_slice(run.text.as_bytes());
                        acc.push(
                            run.style.bold as u8
                                | (run.style.italic as u8) << 1
                                | (run.style.code as u8) << 2
                                | (run.style.strikethrough as u8) << 3
                                | (run.style.link.is_some() as u8) << 4,
                        );
                    }
                    acc.push(b'\n');
                }
            }
            Some(ToolDetail::Diff { file, .. }) => {
                acc.push(2);
                acc.extend_from_slice(file.path.as_bytes());
                acc.extend_from_slice(&file.additions.to_le_bytes());
                acc.extend_from_slice(&file.deletions.to_le_bytes());
                acc.extend_from_slice(&(file.hunks.len() as u32).to_le_bytes());
            }
            Some(ToolDetail::Stats { stats }) => {
                acc.push(3);
                for stat in stats.iter() {
                    acc.extend_from_slice(stat.path.as_bytes());
                    acc.extend_from_slice(&stat.additions.to_le_bytes());
                    acc.extend_from_slice(&stat.deletions.to_le_bytes());
                }
            }
        }
        // The invocation block is pure over `call`, which the one-line hash
        // above only covers by length — hash its bytes so an in-place call
        // update (a streaming MCP input, a growing todo list) re-splices.
        if let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = t.invocation.as_deref()
        {
            for line in lines {
                acc.extend_from_slice(line.as_bytes());
            }
            acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
        }
        // Sidecar refs arriving after the resolve tick must re-splice too —
        // they add the fetch affordance without changing the detail payload.
        acc.push(t.output_ref.is_some() as u8 | (t.diff_ref.is_some() as u8) << 1);
        // Subagent lifecycle mutates the chip in place (status flips, the
        // live tail grows) — hash it so the row re-splices on every change.
        acc.push(
            t.subagent_ref.is_some() as u8
                | match t.subagent_status {
                    None => 0,
                    Some(SubagentStatus::Running) => 1 << 1,
                    Some(SubagentStatus::Done) => 2 << 1,
                    Some(SubagentStatus::Failed) => 3 << 1,
                },
        );
        if let Some(tail) = &t.subagent_tail {
            acc.extend_from_slice(tail.as_bytes());
        }
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Clipboard payload for an assistant/system entry: authored text parts in
/// document order, preserving Markdown while excluding tool traces and other
/// structured parts.
fn assistant_copy_text(entry: &SessionMessageEntry) -> Option<SharedString> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            // Inspect the trimmed view only to reject empty parts. Copy the
            // original bytes so indentation-based code blocks and Markdown
            // hard-break whitespace survive the clipboard round trip.
            MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then(|| text.into())
}

/// Conservative first-frame fallback for whether a prompt may need a fold
/// affordance. Once the text element has measured, `render_user_body` replaces
/// this proxy with the exact wrapped-line count, so glyph width and script no
/// longer affect eligibility heuristically.
pub fn user_message_needs_collapse(text: &str) -> bool {
    text.lines().count() > USER_COLLAPSED_LINES || text.chars().count() > USER_COLLAPSE_CHARS
}

/// Layout transitions need more time when they travel farther, otherwise a
/// 1,000px pasted log crosses the screen in the same 200ms as a six-line note
/// and reads as a snap. Keep ordinary messages close to the catalog RESIZE
/// timing, then scale to an 850ms ceiling for genuinely large pasted content.
pub fn user_resize_duration_ms(height_delta: f32) -> u64 {
    (220.0 + height_delta.max(0.0) * 0.32).min(850.0).round() as u64
}

/// Short folds keep the app's familiar decisive ease-out. Large folds use
/// ease-in-out so thousands of pixels do not disappear in the first few frames
/// of an aggressively front-loaded curve.
pub fn user_resize_spec(height_delta: f32) -> motion::MotionSpec {
    let curve = if height_delta > 500.0 {
        motion::EASE_IN_OUT
    } else {
        motion::EASE_OUT
    };
    motion::MotionSpec::new(user_resize_duration_ms(height_delta), curve)
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
    let entry_id: SharedString = entry.id.clone().into();

    if entry.role == MessageRole::User {
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // Attachment refs ride the plain text (the `withAttachments`
        // transport); split them back out for the thumbnail strip.
        let parsed = crate::attachments::parse_user_message_images(&raw);
        // File mentions render as chips here too, not just in the composer.
        // The projection is pure over the text, so the raw-length row version
        // below stays a valid cache/diff key.
        // Lifted before the mention projection, so a comment body's own
        // Markdown never lands in the bubble.
        let (body, badges) = crate::badges::split(&parsed.text);
        let (text, mentions) = match crate::composer::sent_mention_display(&body) {
            Some((display, spans)) => (display, spans),
            None => (body, Vec::new()),
        };
        let copy_text = (!text.trim().is_empty()).then(|| SharedString::from(text.clone()));
        return vec![Row {
            id: entry.id.clone().into(),
            version: (raw.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                text: text.into(),
                mentions: Arc::new(mentions),
                attachments: Arc::new(parsed.attachments),
                badges: Arc::new(badges),
                pending,
            },
            entry_id,
            // User rows always carry the strip (chat-view.tsx: whenever
            // `createdAt` exists — the optimistic echo included).
            timestamp: Some(entry.created_at),
            copy_text,
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive
    // ordinary tools. Agent/spawn chips flush into their own group so they
    // never share a collapse with Reads/Runs.
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
                entry_id: entry.id.clone().into(),
                timestamp: None,
                copy_text: None,
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } => {
                let item = ToolItem {
                    call: call.clone(),
                    is_error: *is_error,
                    resolved: *resolved,
                    detail: tool_detail(output.as_deref(), diff.as_ref(), diff_stats.as_deref())
                        .map(Arc::new),
                    invocation: call_block(call).map(Arc::new),
                    output_ref: output_ref.clone().map(SharedString::from),
                    output_bytes: *output_bytes,
                    diff_ref: diff_ref.clone().map(SharedString::from),
                    subagent_ref: subagent_ref.clone().map(SharedString::from),
                    subagent_status: *subagent_status,
                    subagent_tail: subagent_tail.clone().map(SharedString::from),
                    is_thought: false,
                };
                // Agent chips don't share a fold with ordinary tools: flush
                // whenever the genus flips so each group is uniform.
                if pending_group
                    .first()
                    .is_some_and(|head| is_agent_tool(head) != is_agent_tool(&item))
                {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            // Thinking rides the SAME accordion as the tools around it
            // (user request) — a thought chip in the group, not its own row.
            MessagePart::Reasoning { id: part_id, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                // Live only while it is the tail of a streaming reply — once
                // text or a tool follows, the thought is finished even though
                // the entry still streams.
                let live = streaming && part_ix == last_part_ix;
                // The same parse wiring as text parts: incremental while
                // streaming, hanging inline markers mended for display, the
                // settled cache once complete.
                let tree = parse(&format!("{}#{}", entry.id, part_id), text);
                let item = thought_item(&tree, live);
                // Thoughts join ordinary tool groups; agent (spawn-link)
                // groups stay pure, exactly like the tool genus rule.
                if pending_group.first().is_some_and(is_agent_tool) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
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
                        // Live and completed parts split identically — one row
                        // per top-level block, same ids, so the live→complete
                        // handoff never changes row identity. The version is a
                        // content hash of the block's bytes (LSB = streaming),
                        // so a commit only splices rows whose bytes actually
                        // changed — the settled prefix of a live reply is
                        // untouched (and its render caches stay valid).
                        for block_ix in 0..tree.blocks.len() {
                            let range = &tree.blocks[block_ix].range;
                            let end = range.end.min(text.len());
                            let bytes = text
                                .as_bytes()
                                .get(range.start.min(end)..end)
                                .unwrap_or_default();
                            let version = (fnv1a(bytes) << 1) | streaming as u64;
                            rows.push(Row {
                                id: format!("{key}.{block_ix}").into(),
                                version,
                                turn_start: false,
                                entry_id: entry_id.clone(),
                                timestamp: None,
                                copy_text: None,
                                kind: if streaming {
                                    RowKind::LiveMarkdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                } else {
                                    RowKind::Markdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                },
                            });
                        }
                    }
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        // Model-generated header onto the one-line chip.
                        let header: SharedString = single_line(
                            &questions
                                .first()
                                .map(|q| q.header.clone())
                                .unwrap_or_else(|| "Question".to_string()),
                        )
                        .into();
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                header,
                                resolved: *resolved,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
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
                                // Harness-generated; the chip is one line.
                                message: single_line(message).into(),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    // Tools and thoughts are grouped by the outer arms;
                    // nothing reaches here.
                    MessagePart::Tool { .. } | MessagePart::Reasoning { .. } => {}
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
    // Timestamp strip under the entry's LAST row once the turn has settled
    // (chat-view.tsx: "No timestamp hover mid-stream"). The version bit keeps
    // the diff key honest for last-row kinds whose own version wouldn't
    // change when streaming flips off (chips).
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.copy_text = assistant_copy_text(entry);
        last.version ^= 1 << 62;
    }
    rows
}

/// `ZERON_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("ZERON_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

const FRAME_STATS_WINDOW: usize = 240;

/// Opt-in cadence counters complement per-row timings: inexpensive rows can
/// still exhaust a battery when an unrelated animation rebuilds them at 120Hz.
pub(crate) fn record_view_frame(view: &'static str) -> bool {
    if !frame_stats_enabled() {
        return false;
    }
    thread_local! {
        static COUNTERS: RefCell<HashMap<&'static str, (Instant, u64)>> = RefCell::default();
    }
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        let (start, frames) = counters.entry(view).or_insert_with(|| (Instant::now(), 0));
        *frames += 1;
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            tracing::warn!(
                view,
                frames_per_second = *frames as f64 / elapsed,
                "view render cadence"
            );
            *start = Instant::now();
            *frames = 0;
            return true;
        }
        false
    })
}

/// `ZERON_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("ZERON_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

/// How [`parse_for_row`] produced its tree — carries the incremental parser's
/// work counters so callers (and tests) can see that per-append parse work is
/// bounded by the reparsed tail, never the whole accumulated reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live [`IncrementalParser`] advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched (render caches stay valid).
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache (no parse at all).
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring, extracted for testability: one call
/// per text part per sync. Streaming parts keep one [`IncrementalParser`] per
/// row key and advance it with the full accumulated text (`set_text` takes the
/// O(tail) append path for the prefix-extensions the doc watch delivers);
/// completed parts hit the settled cache, adopt the live parser's tree on the
/// live→complete flip (flicker-free handoff), or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            // Display tree: hanging inline markers mended so closers arriving
            // later never reflow painted text (markdown/mend.rs). Completed
            // rows below use the canonical tree — the honest settle.
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        // On the live→complete flip reuse the live parser's tree when
        // the sources match — the split rows then share the exact tree
        // the unsplit row painted, guaranteeing a flicker-free handoff.
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel. Tool groups get one larger global
/// step on either boundary so their dense chip stack has room to breathe.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return Theme::SPACE_LG;
    }
    let is_md = |k: &RowKind| matches!(k, RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. });
    let same_part_markdown = prev.is_some_and(|p| {
        is_md(&p.kind) && is_md(&row.kind) && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else if matches!(row.kind, RowKind::ToolGroup { .. })
        || prev.is_some_and(|row| matches!(row.kind, RowKind::ToolGroup { .. }))
    {
        Theme::SPACE_MD
    } else {
        Theme::SPACE_SM
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

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// The rule lives in `zeron_proto::view` so the terminal viewport reports the
/// same summary; this only adapts the row model's [`ToolItem`] to it.
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools
        .iter()
        .filter(|t| !t.is_thought)
        .map(|t| (t.call.clone(), t.is_error))
        .collect();
    let thoughts = tools.iter().filter(|t| t.is_thought).count();
    // The shared summary answers "used 0 tools" for an empty set — a
    // thought-only group must not inherit that.
    let base = if pairs.is_empty() {
        String::new()
    } else {
        zeron_proto::view::tool_group_summary(&pairs)
    };
    // Thought chips ride the group (they are UI-synthesized, so the shared
    // view summary never sees them): name them on the collapsed line.
    match (base.is_empty(), thoughts) {
        (_, 0) => base,
        (true, 1) => "Thought process".into(),
        (true, n) => format!("Thought {n} times"),
        (false, 1) => format!("Thought · {base}"),
        (false, n) => format!("Thought {n} times · {base}"),
    }
}

// `single_line` and the per-kind chip label/detail are shared with the terminal
// viewport (`zeron_proto::view`): a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use zeron_proto::view::{single_line, tool_chip_content};

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

/// Analytic height an open detail adds to its chip's card (separator + body)
/// — output blocks by line count, diff blocks via the changes pane's own
/// [`crate::changes::body_height`]. The chip's own [`CHIP_HEIGHT`] is already
/// counted by [`chips_height`].
pub fn detail_height(detail: &ToolDetail) -> f32 {
    let body = match detail {
        ToolDetail::Output {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Diff { file, .. } => crate::changes::body_height(file),
        ToolDetail::Stats { stats } => stats.len() as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD,
    };
    DETAIL_SEPARATOR + body
}

/// Height of the "Show full output/diff" affordance row appended below an
/// open detail whose full payload lives in the sidecar (chat2-sync A3).
pub const BLOB_AFFORDANCE_HEIGHT: f32 = 24.0;

/// What an open chip's [`BLOB_AFFORDANCE_HEIGHT`] row offers: a lazy sidecar
/// fetch ("Show full output/diff"). One slot, so the analytic height sums
/// stay a single `is_some` check.
#[derive(Clone)]
struct ChipAffordance {
    blob_ref: SharedString,
    label: SharedString,
}

/// Line cap for a FETCHED full output (a defensive ceiling, not a doc cap —
/// the harness bounds outputs at 4KiB, so this is rarely reached).
const FULL_OUTPUT_MAX_LINES: usize = 400;

/// Build the upgraded detail from a fetched sidecar blob. Diff blobs parse
/// the `ToolDiff` JSON through the same pipeline as inline diffs; output
/// blobs render (near-)uncapped — fetching past the summary was the point.
fn blob_detail(text: &str, is_diff: bool) -> Option<ToolDetail> {
    if is_diff {
        let diff: zeron_proto::ToolDiff = serde_json::from_str(text).ok()?;
        return tool_detail(None, Some(&diff), None);
    }
    let mut lines: Vec<SharedString> = text
        .lines()
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(FULL_OUTPUT_MAX_LINES);
    lines.truncate(FULL_OUTPUT_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Compact byte size for the fetch affordance label ("812 B", "12 KB").
fn format_kb(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
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

/// The working trailer's "Sending…" bridge: true while an in-flight send is
/// fresher than the session row's turn start — the row still carries the
/// PREVIOUS turn (or none), so a timer would count the send round-trip and
/// restart when the turn actually begins.
pub fn sending_bridge(
    send_started: Option<chrono::DateTime<chrono::Utc>>,
    turn_started: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    match (send_started, turn_started) {
        (Some(send), Some(turn)) => turn <= send,
        (Some(_), None) => true,
        (None, _) => false,
    }
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

struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Option<Weak<zeron_syntax::HighlightedDocument>>,
    _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
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
    ) -> Option<Arc<zeron_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.as_ref()?;
            if let Some(document) = document.upgrade() {
                return Some(document);
            }
        }
        if let Some(document) = self.cache.get(&document_key) {
            self.entries.insert(
                slot_key,
                HighlightEntry {
                    key: document_key,
                    document: Some(Arc::downgrade(&document)),
                    _task: None,
                },
            );
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                        source: &code,
                        path: None,
                        fence_tag: Some(match lang {
                            Lang::Rust => "rust",
                            Lang::JavaScript => "javascript",
                            Lang::Jsx => "jsx",
                            Lang::TypeScript => "typescript",
                            Lang::Tsx => "tsx",
                            Lang::Python => "python",
                            Lang::Go => "go",
                            Lang::Json => "json",
                            Lang::Jsonc => "jsonc",
                            Lang::Bash => "bash",
                            Lang::Toml => "toml",
                            Lang::Markdown => "markdown",
                            Lang::Html => "html",
                            Lang::Css => "css",
                            Lang::Yaml => "yaml",
                            Lang::C => "c",
                            Lang::Cpp => "cpp",
                            Lang::CSharp => "csharp",
                            Lang::Java => "java",
                            Lang::Kotlin => "kotlin",
                            Lang::Swift => "swift",
                            Lang::Ruby => "ruby",
                            Lang::Php => "php",
                            Lang::Sql => "sql",
                            Lang::Lua => "lua",
                            Lang::Dockerfile => "dockerfile",
                            Lang::Nix => "nix",
                            Lang::Make => "make",
                        }),
                    })
                    .ok()
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(document) = document {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key)
                        && entry.key == document_key
                    {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                _task: Some(task),
            },
        );
        None
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
    /// When the toggle happened. The tween is armed only for a short window
    /// after the click: gpui replays an element's animation on REMOUNT, and a
    /// virtualized row scrolling back into view is a remount — an armed-forever
    /// tween made every once-collapsed group flash open→closed on each
    /// reappearance (user report).
    toggled_at: Option<Instant>,
    /// Per-toggle duration. User bubbles scale this with travel distance;
    /// existing tool folds leave it at zero and keep their catalog constants.
    duration_ms: u64,
}

/// Viewport compensation paired with a long user-message collapse. While the
/// row loses height, this scrolls upward by the same eased distance so a
/// bottom-pinned viewport keeps the collapsing bubble in view.
struct UserCollapseScroll {
    started_at: Instant,
    duration_ms: u64,
    height_delta: f32,
    row_ix: usize,
    initial_top: f32,
    target_top: f32,
}

/// A locally-sent turn reserves the viewport below its prompt. The last row
/// has a minimum height, so streaming content and the working trailer consume
/// or release space in the same layout pass. Only changes to the preceding
/// rows require a post-layout refinement. Wheel input releases the automatic
/// glide/hold while preserving the reservation; overflow hands off to the
/// ordinary bottom spring. Chat switches restore the reservation released.
#[derive(Clone, Debug)]
struct OwnTurnAnchor {
    chat_id: String,
    message_id: SharedString,
    /// The step still owns the viewport (glide → hold). Any wheel/touch
    /// input releases it — the reservation stays behind as plain scrollable
    /// space, and the ordinary escape/restick rules apply from then on.
    held: bool,
    /// The entry glide has landed; the hold now re-asserts the prompt's
    /// position absolutely after every layout (glue- and lag-proof — the
    /// exact mechanism the shipped first-send anchor used).
    positioned: bool,
    /// A fresh send may install the anchor one notification before its echo.
    /// Once the prompt has appeared, its later disappearance is terminal
    /// (failed echo or removed entry) and the runway must retire.
    seen_prompt: bool,
}

impl OwnTurnAnchor {
    fn released_for_restore(mut self) -> Self {
        self.held = false;
        self.positioned = false;
        self.seen_prompt = true;
        self
    }

    fn observe_prompt(&mut self, exists: bool) -> bool {
        if exists {
            self.seen_prompt = true;
        }
        exists || !self.seen_prompt
    }
}

/// A stable per-chat viewport anchor. Row identity is preferred over its old
/// index because async replay can insert or remove rows while a chat is away.
#[derive(Clone, Debug)]
struct ViewportAnchor {
    row_id: SharedString,
    entry_id: SharedString,
    fallback_ix: usize,
    offset_in_row: Pixels,
}

impl ViewportAnchor {
    fn capture(rows: &[Row], scroll_top: ListOffset) -> Option<Self> {
        let fallback_ix = scroll_top.item_ix.min(rows.len().checked_sub(1)?);
        let row = &rows[fallback_ix];
        Some(Self {
            row_id: row.id.clone(),
            entry_id: row.entry_id.clone(),
            fallback_ix,
            offset_in_row: scroll_top.offset_in_item,
        })
    }

    fn resolve_exact(&self, rows: &[Row]) -> Option<ListOffset> {
        let item_ix = rows.iter().position(|row| row.id == self.row_id)?;
        Some(ListOffset {
            item_ix,
            offset_in_item: self.offset_in_row,
        })
    }

    fn resolve(&self, rows: &[Row]) -> Option<ListOffset> {
        if let Some(offset) = self.resolve_exact(rows) {
            return Some(offset);
        }

        // A row can disappear when a streaming block is reshaped. Stay in the
        // same message entry, choosing the surviving row nearest the old
        // location; the intra-row offset is no longer meaningful in that case.
        let item_ix = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry_id == self.entry_id)
            .min_by_key(|(ix, _)| ix.abs_diff(self.fallback_ix))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| self.fallback_ix.min(rows.len().saturating_sub(1)));
        (!rows.is_empty()).then_some(ListOffset {
            item_ix,
            offset_in_item: px(0.0),
        })
    }
}

/// Session-local viewport state. Chats that were following their tail keep
/// following it; only user-owned viewports restore a concrete row anchor.
#[derive(Clone, Debug)]
enum SavedViewport {
    FollowTail,
    Anchored {
        anchor: ViewportAnchor,
        distance_from_bottom: f32,
        /// Preserve the runway that made a short active turn scrollable.
        /// Navigation releases its automatic hold, so revisiting restores the
        /// viewport without immediately following new output to the bottom.
        own_turn: Option<OwnTurnAnchor>,
    },
}

struct RestoredViewport {
    offset: ListOffset,
    distance_from_bottom: f32,
    own_turn: Option<OwnTurnAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewportFinalizeToken {
    generation: u64,
    layout_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptReplayState {
    Pending,
    Empty,
    Populated,
}

impl TranscriptReplayState {
    fn authoritative_empty(self) -> bool {
        self == Self::Empty
    }

    fn allows_fallback(self) -> bool {
        self == Self::Populated
    }
}

impl ViewportFinalizeToken {
    fn still_current(self, generation: u64) -> bool {
        self.generation == generation
    }

    fn layout_settled(self, layout_revision: u64) -> bool {
        self.layout_revision == layout_revision
    }
}

impl SavedViewport {
    fn capture(
        rows: &[Row],
        scroll_top: ListOffset,
        pinned: bool,
        distance_from_bottom: f32,
        own_turn: Option<&OwnTurnAnchor>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if pinned {
            return Some(Self::FollowTail);
        }
        Some(Self::Anchored {
            anchor: ViewportAnchor::capture(rows, scroll_top)?,
            distance_from_bottom,
            own_turn: own_turn.cloned(),
        })
    }

    /// Before the opening reset arrives, rows may contain only optimistic
    /// echoes. In that gap an exact row is safe, but entry/index fallbacks
    /// would mistake an unrelated echo for the authoritative transcript.
    fn resolve(&self, rows: &[Row], allow_fallback: bool) -> Option<RestoredViewport> {
        let Self::Anchored {
            anchor,
            distance_from_bottom,
            own_turn,
        } = self
        else {
            return None;
        };
        let offset = if allow_fallback {
            anchor.resolve(rows)?
        } else {
            anchor.resolve_exact(rows)?
        };
        let own_turn = own_turn
            .clone()
            .filter(|turn| {
                rows.iter()
                    .any(|row| row.turn_start && row.entry_id == turn.message_id)
            })
            .map(OwnTurnAnchor::released_for_restore);
        Some(RestoredViewport {
            offset,
            distance_from_bottom: *distance_from_bottom,
            own_turn,
        })
    }
}

#[derive(Default)]
struct SavedViewportCache {
    by_chat: HashMap<String, SavedViewport>,
    recency: VecDeque<String>,
}

#[derive(Default)]
struct CodeFenceRuntime {
    scroll: ScrollHandle,
    scrollbar: crate::popover::HorizontalScrollbarState,
}

impl SavedViewportCache {
    fn insert(&mut self, chat_id: String, viewport: SavedViewport) {
        if self.by_chat.contains_key(&chat_id) {
            self.recency.retain(|candidate| candidate != &chat_id);
        }
        self.recency.push_back(chat_id.clone());
        self.by_chat.insert(chat_id, viewport);
        while self.by_chat.len() > MAX_SAVED_VIEWPORTS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.by_chat.remove(&evicted);
        }
    }

    fn get_cloned_and_touch(&mut self, chat_id: &str) -> Option<SavedViewport> {
        let viewport = self.by_chat.get(chat_id).cloned()?;
        self.recency.retain(|candidate| candidate != chat_id);
        self.recency.push_back(chat_id.to_string());
        Some(viewport)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_chat.len()
    }
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    last_source: Option<(Option<String>, TranscriptReplayState, u64)>,
    chat_id: Option<String>,
    /// `Some(doc_id)` pins this instance to a SUBAGENT doc: rows come from
    /// `AppState::sub_transcript(doc_id)` instead of the selected chat, and
    /// the instance is READ-ONLY — no echoes, no own-turn hold, and no global
    /// attachment protection (that set is shared with the primary transcript
    /// and overwritten wholesale).
    doc_override: Option<String>,
    /// Whether an override instance watches a LIVE doc (`for_doc(follow)`):
    /// only then may the working trailer render — a frozen snapshot must
    /// never spin, whatever its entries claim.
    doc_live: bool,
    /// Memory-only viewport state for primary chats visited in this window.
    /// A transcript instance is shared across tabs, so the active ListState is
    /// reset on every attach and cannot retain these positions by itself.
    saved_viewports: SavedViewportCache,
    /// An anchored viewport waiting for the selected chat's async replay.
    pending_viewport: Option<SavedViewport>,
    /// Generation of the selected chat, guarding post-layout restoration
    /// callbacks across rapid A→B→A navigation.
    viewport_generation: u64,
    /// A restored item anchor needs one post-layout refresh of distance-based
    /// UI state; programmatic list scrolling never invokes `handle_scroll`.
    viewport_finalize_pending: bool,
    viewport_finalize_scheduled: bool,
    /// Bumped whenever sync or own-turn logic invalidates measured rows. The
    /// post-restore finalizer waits until one layout completes without another
    /// invalidation, avoiding a stale jump-button decision.
    viewport_layout_revision: u64,
    /// One-shot "open at the latest content" for UNPINNED (frozen) override
    /// instances: rows land ASYNC after the tab opens (watch replay / blob
    /// fetch), so the end-scroll fires on the first non-empty sync, then
    /// never again — landing at the end and FOLLOWING it are different
    /// states, and the user owns the viewport from there. Pinned instances
    /// don't need it (the pin branch already opens at the end).
    land_end_pending: bool,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Detail folds (output/diff) per chip, keyed `"{row_id}#d{ix}"` — full
    /// [`FoldState`]s so detail bodies tween open/closed exactly like the
    /// group fold. Render-local like `folds` — never part of the row
    /// fingerprint.
    tool_details: HashMap<SharedString, FoldState>,
    /// Expand/collapse state for user bubbles past [`USER_COLLAPSED_LINES`],
    /// keyed by row id. Render-local like `folds` — never part of the row
    /// fingerprint, so toggling one costs a repaint, not a rebuild.
    user_folds: HashMap<SharedString, FoldState>,
    /// Full laid-out text heights for long user bubbles. The text's paint
    /// canvas writes these cells without notifying or mutating the transcript;
    /// click handlers read them as exact endpoints for the RESIZE tween. This
    /// preserves smooth layout motion without a paint → notify feedback loop.
    user_heights: HashMap<SharedString, Rc<Cell<f32>>>,
    /// Pending long-press toggle. A single task is enough because only one
    /// pointer can own a hold gesture at a time; a token invalidates stale
    /// timers when the pointer is released or moves into a text selection.
    user_hold_task: Option<Task<()>>,
    user_hold_token: u64,
    user_collapse_scroll: Option<UserCollapseScroll>,
    /// Tracks the queued frame, even when its animation is canceled/replaced.
    /// Only that callback clears it, so rapid input cannot fork frame drivers.
    user_collapse_scroll_scheduled: bool,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    rendered_rows: HashSet<SharedString>,
    /// Last UI typography generation reflected in `list` item measurements.
    /// Family and size changes can alter prose wrapping without changing row
    /// identity, so the virtual list must explicitly discard cached heights.
    typography_generation: u32,
    /// Last global code-fence layout generation applied to this transcript.
    /// Each instance owns separate scroll handles and list measurements, so
    /// every one must reset itself after a global Fit-mode transition.
    code_fences_generation: u64,
    highlights: HighlightStore,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, after an own-send first overflows, and
    /// on the jump button.
    pinned: bool,
    /// A locally-sent prompt currently held near the viewport top while its
    /// reply grows into the empty space below it.
    own_turn: Option<OwnTurnAnchor>,
    /// A layout-affecting change needs one post-layout own-turn measurement.
    own_turn_kick: bool,
    /// One own-turn `on_next_frame` callback in flight at most.
    own_turn_scheduled: bool,
    /// Wall-clock of the previous entry-glide tick (`None` = not gliding).
    own_turn_last_tick: Option<Instant>,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// Last pointer sample while markdown selection owns a left-button drag.
    selection_drag_position: Option<Point<Pixels>>,
    /// One-shot timer rescheduled only while the pointer remains in an edge
    /// zone. Dropping it on mouse-up stops all selection scroll work.
    selection_scroll_task: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Height of the shell's composer/status/terminal stack overlaying the
    /// transcript's bottom (measured last frame): the last row pads past it
    /// so pinned content rests above the glass chrome it scrolls under.
    bottom_clearance: f32,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer — reveals the entry's timestamp
    /// strip (zeron chat-view.tsx `group-hover`; the rows report hover
    /// themselves). Keyed by ROW so a row→row move within one entry can't
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Per-visible-fence horizontal offsets and scrollbar hover/drag state.
    /// Keys use the transcript's stable row identity, so streaming → settled
    /// rerenders keep their local scroll position without leaking state for
    /// blocks no longer present in the selected chat.
    code_fences: HashMap<SharedString, CodeFenceRuntime>,
    /// Entry whose hover action is showing transient copied-check feedback.
    copied_message: Option<SharedString>,
    copied_message_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// Focused while the lightbox is open so Escape reaches it.
    attachment_preview_focus: gpui::FocusHandle,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(String, String), Task<()>>,
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(String, String), Task<()>>,
    /// Sidecar blob fetches keyed by doc ref (`chatId/partId[.diff]`,
    /// chat2-sync A3). `Ready` holds the UPGRADED detail, built once on
    /// arrival — render swaps it in per chip; rows never rebuild for it.
    /// Deliberately NOT cleared on chat switch: refs are chat-qualified and a
    /// fetched blob stays valid.
    blob_details: HashMap<SharedString, BlobFetch>,
    /// Monotonic fetch order per blob ref: when a tool has BOTH a diff and
    /// an output blob fetched, the chip shows the one requested most
    /// recently (click "Show full output" after a diff → see the output).
    blob_fetch_order: HashMap<SharedString, u64>,
    blob_fetch_counter: u64,
    _observe: Subscription,
    _text_changes: Subscription,
}

/// One sidecar blob fetch's lifecycle.
enum BlobFetch {
    Loading(#[allow(dead_code)] Task<()>),
    /// Failed with the affordance re-armed as a retry.
    Failed,
    Ready(Arc<ToolDetail>),
}

/// Shell-facing events (the transcript itself hosts no surfaces).
#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    /// A spawn chip's "Open subagent" affordance: open the subagent's
    /// transcript as a right-pane tab. `chat_id` is the doc the chip lives
    /// in (the frozen blob is keyed `{chat_id}/{doc_id}`); `frozen` means
    /// the subagent finished — try the blob before watching the doc.
    OpenSubagent {
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
    },
}

impl gpui::EventEmitter<TranscriptEvent> for Transcript {}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self::build(state, None, true, cx)
    }

    /// A read-only transcript over one SUBAGENT doc (right-pane tab). The
    /// caller starts the feed (`watch_subagent_doc` or the frozen snapshot);
    /// this instance only renders whatever lands under `doc_id`. `follow` =
    /// the doc is live: engage the end-follow pin from the start. Either
    /// way the tab OPENS at the latest content — a frozen transcript lands
    /// at the end once, unpinned, and free-scrolls from there.
    pub fn for_doc(
        state: Entity<AppState>,
        doc_id: String,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(state, Some(doc_id), follow, cx)
    }

    fn build(
        state: Entity<AppState>,
        doc_override: Option<String>,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // FollowMode stays Normal: the tail pin is ours (a per-frame spring),
        // not the list's per-layout hard snap.
        //
        // Override instances align TOP: a subagent transcript reads like a
        // fresh notes page — entries anchored at the top, streaming growing
        // into the empty space below, never rising from the pane's bottom.
        // Top alignment gets that structurally (a short list rests at the
        // top with no reservation pad), and the PIN machinery still runs on
        // top of it for end-follow: the spring is purely distance-based, and
        // the glue trap it was built around is Bottom-only — layout
        // materializes a Top list's past-end offset to a CONCRETE position
        // every frame (gpui list.rs: only `Bottom` re-glues to the `None`
        // sentinel), so a parked spring can't re-glue and hard-track growth.
        let alignment = if doc_override.is_some() {
            ListAlignment::Top
        } else {
            ListAlignment::Bottom
        };
        let list = ListState::new(0, alignment, px(OVERDRAW_PX));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let text_changes = cx.subscribe(
            &state,
            |this: &mut Self, state, event: &crate::state::TranscriptTextChanged, cx| {
                let doc_id = this
                    .doc_override
                    .as_deref()
                    .or_else(|| state.read(cx).selected_chat.as_deref());
                if doc_id == Some(event.doc_id.as_str()) {
                    this.sync(cx);
                }
            },
        );
        // The rail is sized for the conversation column; a narrow right-pane
        // tab has no width gate driving it, so override instances skip it.
        let rail_enabled = doc_override.is_none();
        // `follow` is the initial pin: the primary transcript always opens
        // pinned; an override instance pins only while its doc is LIVE (a
        // frozen transcript reads top-down, free-scrolling). Short content
        // is at-end by definition (distance 0), so the pin is invisible
        // until streaming overflows the pane — then it follows, releases on
        // wheel-up, and resticks/jumps exactly like the main transcript.
        let pinned = follow;
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            last_source: None,
            // Pre-set so `sync` never sees an attach edge — an override
            // instance must not reset (or re-pin) on selection changes.
            chat_id: doc_override.clone(),
            land_end_pending: doc_override.is_some() && !follow,
            doc_live: doc_override.is_some() && follow,
            doc_override,
            saved_viewports: SavedViewportCache::default(),
            pending_viewport: None,
            viewport_generation: 0,
            viewport_finalize_pending: false,
            viewport_finalize_scheduled: false,
            viewport_layout_revision: 0,
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            tool_details: HashMap::new(),
            user_folds: HashMap::new(),
            user_heights: HashMap::new(),
            user_hold_task: None,
            user_hold_token: 0,
            user_collapse_scroll: None,
            user_collapse_scroll_scheduled: false,
            veils: HashMap::new(),
            veil_baseline: std::collections::HashSet::new(),
            veil_attach_pending: true,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            rendered_rows: HashSet::new(),
            typography_generation: crate::typography::generation(cx),
            code_fences_generation: crate::settings::code_fences_generation(cx),
            highlights: HighlightStore::default(),
            show_jump_button: false,
            last_scroll_distance: 0.0,
            pinned,
            own_turn: None,
            own_turn_kick: false,
            own_turn_scheduled: false,
            own_turn_last_tick: None,
            spring: StickSpring::new(),
            spring_last_tick: None,
            spring_settled_at: None,
            spring_kick: false,
            spring_scheduled: false,
            scroll_anim: None,
            selection_drag_position: None,
            selection_scroll_task: None,
            rail_enabled,
            bottom_clearance: 0.0,
            rail_hover: None,
            hovered_entry: None,
            copied_code: None,
            copied_clear: None,
            code_fences: HashMap::new(),
            copied_message: None,
            copied_message_clear: None,
            attachment_preview: None,
            attachment_preview_focus: cx.focus_handle(),
            attachment_loads: HashMap::new(),
            attachment_retries: HashMap::new(),
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
            _observe: observe,
            _text_changes: text_changes,
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

    /// Shell-driven: the measured height of the bottom chrome stack the
    /// transcript scrolls under. Sub-pixel jitter is ignored so steady-state
    /// frames don't re-notify.
    pub fn set_bottom_clearance(&mut self, height: f32, cx: &mut Context<Self>) {
        if (self.bottom_clearance - height).abs() > 0.5 {
            self.bottom_clearance = height;
            if self.own_turn.is_some() {
                self.remeasure_last_row();
                self.own_turn_kick = true;
            }
            cx.notify();
        }
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

    /// Snapshot the outgoing primary chat before its rows and ListState are
    /// reset. Empty rows never overwrite an older snapshot: during a rapid
    /// A→B→A switch, B's replay may not have arrived before leaving it again.
    fn remember_current_viewport(&mut self) {
        // Rows can already contain optimistic echoes while an older snapshot
        // is still waiting for the authoritative replay. Leaving again in
        // that window must preserve the older snapshot, not replace it with
        // the partial echo-only viewport.
        if self.pending_viewport.is_some() {
            return;
        }
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };
        let distance_from_bottom = if self.pinned {
            0.0
        } else {
            self.distance_from_bottom()
        };
        let Some(viewport) = SavedViewport::capture(
            &self.rows,
            self.list.logical_scroll_top(),
            self.pinned,
            distance_from_bottom,
            self.own_turn.as_ref(),
        ) else {
            return;
        };
        self.saved_viewports.insert(chat_id, viewport);
    }

    /// Restore an exact optimistic row while replay is pending, enable stable
    /// fallbacks only after a populated reset, and retire snapshots proven
    /// absent by an empty reset. `scroll_to` remains valid while the virtual
    /// list measures restored rows on the following layout pass.
    fn restore_pending_viewport(&mut self, replay: TranscriptReplayState) -> bool {
        if self.pending_viewport.is_none() {
            return false;
        }
        if !self.rows.is_empty()
            && let Some(restored) = self
                .pending_viewport
                .as_ref()
                .and_then(|saved| saved.resolve(&self.rows, replay.allows_fallback()))
        {
            self.pending_viewport = None;
            self.list.scroll_to(restored.offset);
            self.own_turn = restored.own_turn;
            self.own_turn_kick = self.own_turn.is_some();
            self.own_turn_last_tick = None;
            if self.own_turn.is_some() {
                // Replay readiness can change while echo rows stay identical,
                // so the no-diff path may install a runway without splicing.
                self.remeasure_last_row();
            }
            self.last_scroll_distance = restored.distance_from_bottom;
            self.show_jump_button = restored.distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
            self.viewport_finalize_pending = true;
            return true;
        }

        if !replay.authoritative_empty() {
            return false;
        }
        // The reset's document rows, not the combined rows, define
        // authoritative emptiness. A matching optimistic row above remains
        // valid, but an unrelated echo must never become an index fallback
        // for old history.
        self.discard_pending_viewport();
        if self.own_turn.is_none() {
            self.pinned = true;
            self.last_scroll_distance = 0.0;
            self.show_jump_button = false;
            self.list.scroll_to_end();
        }
        true
    }

    /// Explicit user/navigation intent supersedes a replay-delayed restore.
    /// Replace its cache entry with tail-follow until current rows can be
    /// snapshotted normally on the next chat switch.
    pub(crate) fn discard_pending_viewport(&mut self) {
        if self.pending_viewport.take().is_some()
            && let Some(chat_id) = self.chat_id.clone()
        {
            self.saved_viewports
                .insert(chat_id, SavedViewport::FollowTail);
        }
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Hand viewport ownership to explicit rail/navigation input before its
    /// reduced-motion or animated branch moves the list.
    pub(crate) fn begin_scroll_navigation(&mut self) {
        self.discard_pending_viewport();
        self.cancel_user_hold();
        self.user_collapse_scroll = None;
        // Rail navigation within the session RELEASES the hold but keeps the
        // runway (user spec: only leaving and revisiting the session clears
        // it) — scrolling back down re-arms the hold like any restick.
        self.release_own_turn_hold();
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
    }

    /// Store the animation after [`Self::begin_scroll_navigation`].
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.scroll_anim = Some(task);
    }

    /// Give the viewport to the user/navigation without dropping the
    /// reservation: the pad stays, the hold stands down until a restick.
    fn release_own_turn_hold(&mut self) {
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.held = false;
        }
        self.own_turn_last_tick = None;
    }

    fn remeasure_last_row(&mut self) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.list.remeasure_items(last..last + 1);
            self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
        }
    }

    pub(crate) fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    /// Whether a user scroll should re-engage the bottom pin: inside the 70px
    /// stick band *and* moving toward the bottom. Direction matters — a small
    /// wheel-up notch near the bottom stays inside the band, and re-sticking
    /// on it would snap the view straight back, making the pin unbreakable.
    pub fn should_restick(distance: f32, previous_distance: f32) -> bool {
        distance <= STICK_THRESHOLD_PX && distance < previous_distance
    }

    fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        // Cancel synchronously, before a queued animation frame can undo the
        // wheel/touch input. Neither operation reads the borrowed ListState.
        self.user_collapse_scroll = None;
        self.cancel_user_hold();
        let released_own_turn = self.own_turn.as_ref().is_some_and(|anchor| anchor.held);
        self.release_own_turn_hold();
        if self.own_turn.is_some() {
            // Cancel any tail spring synchronously too; the deferred input
            // decision below may re-engage it after reading the new offset.
            self.pinned = false;
            self.spring.reset();
            self.spring_last_tick = None;
        }
        // The list invokes this handler ONLY from its wheel/touch input path
        // (programmatic scroll_by/scroll_to never re-enter it), while holding
        // its internal RefCell borrow — reading the ListState back
        // synchronously panics with "already mutably borrowed". Defer to the
        // end of the effect cycle, after the list has released its borrow.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this: &mut Transcript, cx| {
                this.discard_pending_viewport();
                // Input owns the viewport immediately, including wheel-down
                // after background streaming. A held turn can be stale while
                // frame callbacks are paused; reasserting its old prompt here
                // made scrolling down impossible until an upward gesture.
                if this.own_turn.is_some() {
                    let distance = this.distance_from_bottom();
                    let previous = this.last_scroll_distance;
                    this.last_scroll_distance = distance;
                    // Reaching the end preserves normal tail-follow intent
                    // without reasserting a possibly stale prompt hold.
                    this.pinned =
                        distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous);
                    this.spring.reset();
                    this.spring_last_tick = None;
                    // Re-stick only when returning to a short turn's actual
                    // hold. An off-screen prompt belongs to an overflowing
                    // reply, even if reservation refinement hasn't run yet.
                    let at_hold = this.own_turn_anchor_ix().is_some_and(|ix| {
                        this.list.bounds_for_item(ix).is_some_and(|bounds| {
                            f32::from(bounds.top() - this.list.viewport_bounds().top())
                                >= Self::own_send_inset(ix) - OWN_SEND_SCROLL_SLACK_PX - 2.0
                        })
                    });
                    if !released_own_turn && at_hold && Self::should_restick(distance, previous) {
                        if let Some(anchor) = this.own_turn.as_mut() {
                            anchor.held = true;
                            anchor.positioned = false;
                        }
                        this.pinned = false;
                        this.own_turn_kick = true;
                    }
                    if this.pinned {
                        this.wake_spring();
                    }
                    this.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX
                        && !this.own_turn.as_ref().is_some_and(|a| a.held);
                    cx.notify();
                    return;
                }
                let distance = this.distance_from_bottom();
                let previous = this.last_scroll_distance;
                this.last_scroll_distance = distance;
                if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                    // User input moving away from the bottom breaks the pin.
                    // Content growth never lands here — it doesn't fire the
                    // scroll handler (mugen §1e: interrupt from input, not
                    // scrollbar position).
                    this.pinned = false;
                    this.spring.reset();
                    this.spring_last_tick = None;
                } else if distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous) {
                    // Returning toward the bottom inside the 70px band (or
                    // arriving at it) re-engages the pin with a glide.
                    if !this.pinned {
                        this.pinned = true;
                        this.wake_spring();
                    }
                }
                let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !this.pinned;
                if show != this.show_jump_button {
                    this.show_jump_button = show;
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn on_selection_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() || !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        self.selection_drag_position = Some(event.position);
        if render::update_drag_at(event.position) {
            cx.notify();
        }
        self.schedule_selection_scroll(cx);
    }

    fn on_selection_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_selection_scroll();
        if let Some(_text) = crate::markdown::selection::end_active_drag() {
            // X11 middle-click paste parity, including the case where the
            // anchor row has virtualized away and cannot receive mouse-up.
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            cx.write_to_primary(ClipboardItem::new_string(_text));
        }
    }

    fn stop_selection_scroll(&mut self) {
        self.selection_drag_position = None;
        self.selection_scroll_task = None;
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_scroll_task.is_some() || !crate::markdown::selection::is_dragging() {
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        if selection_scroll_step(self.list.viewport_bounds(), position) == 0.0 {
            return;
        }
        self.selection_scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECTION_SCROLL_TICK_MS))
                .await;
            let _ = this.update(cx, |transcript, cx| {
                transcript.selection_scroll_task = None;
                transcript.step_selection_scroll(cx);
            });
        }));
    }

    fn step_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        let step = selection_scroll_step(self.list.viewport_bounds(), position);
        if step == 0.0 {
            return;
        }

        // Resolve against the registry painted after the previous step before
        // moving it again. This is what lets a stationary edge pointer consume
        // successive virtualized rows.
        render::update_drag_at(position);
        self.begin_scroll_navigation();
        self.list.scroll_by(px(step));
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        cx.notify();
        self.schedule_selection_scroll(cx);
    }

    /// Reserve the reply's space below a locally-sent prompt — EVERY send,
    /// not just the first (a steer or a post-turn send used to collapse the
    /// previous reservation and drop the messages back down — user report).
    /// [`Self::step_own_turn`] sizes the reservation and eases the prompt to
    /// its top inset. Replacing a previous anchor starts a new glide.
    pub fn on_own_send(&mut self, chat_id: String, message_id: String, cx: &mut Context<Self>) {
        self.user_collapse_scroll = None;
        self.cancel_user_hold();
        self.discard_pending_viewport();
        self.pinned = false;
        self.show_jump_button = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
        // A glued offset re-snaps to the end on EVERY layout — the pad would
        // land and the viewport hard-track its bottom in the same frame,
        // skipping the glide entirely (rig-traced). Pin the offset to a
        // CONCRETE visible item first; the pad then reads as scrollable
        // distance for the glide to cover.
        self.materialize_scroll_anchor();
        let prompt_ix = self
            .rows
            .iter()
            .position(|row| row.turn_start && row.entry_id == message_id.as_str());
        self.own_turn = Some(OwnTurnAnchor {
            chat_id,
            message_id: SharedString::from(message_id),
            held: true,
            positioned: false,
            seen_prompt: prompt_ix.is_some(),
        });
        self.own_turn_last_tick = None;
        self.own_turn_kick = true;
        self.remeasure_last_row();
        cx.notify();
    }

    /// Convert a glued scroll offset (`None`/past-the-end — layout re-snaps
    /// it to the end each frame) into a concrete `{item, offset}` anchored at
    /// the first visible row, which layout holds still.
    fn materialize_scroll_anchor(&mut self) {
        if !self.is_glued() {
            return;
        }
        let vp_top = f32::from(self.list.viewport_bounds().top());
        for ix in 0..self.rows.len() {
            if let Some(bounds) = self.list.bounds_for_item(ix)
                && f32::from(bounds.bottom()) > vp_top + 0.5
            {
                self.list.scroll_to(ListOffset {
                    item_ix: ix,
                    offset_in_item: px(vp_top - f32::from(bounds.top())),
                });
                return;
            }
        }
        // Bottom-aligned short lists expose no item bounds. Materialize
        // their actual end position using the measured height tree instead.
        // Preserve a negative first-row offset for the blank space above a
        // short chat; clamping it to zero would jump as the minimum is added.
        if !self.rows.is_empty() {
            let viewport_height = f32::from(self.list.viewport_bounds().size.height);
            self.list.scroll_by(px(-1.0));
            let content_height = -f32::from(self.list.scroll_px_offset_for_scrollbar().y) + 1.0;
            if content_height < viewport_height {
                self.list.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: px(content_height - viewport_height),
                });
            } else {
                self.list.scroll_by(px(1.0 - viewport_height));
            }
        }
    }

    /// The held prompt's top offset from the viewport top. Row 0 already
    /// carries the titlebar chrome inside its own box (the first row's
    /// top gap), so the hold adds nothing — adding the inset on top parked
    /// a new chat's first prompt a double-chrome ~66px low (user report).
    fn own_send_inset(anchor_ix: usize) -> f32 {
        if anchor_ix == 0 {
            0.0
        } else {
            OWN_SEND_TOP_INSET_PX
        }
    }

    fn own_turn_anchor_ix(&self) -> Option<usize> {
        let anchor = self.own_turn.as_ref()?;
        self.rows
            .iter()
            .position(|row| row.turn_start && row.entry_id == anchor.message_id)
    }

    fn reconcile_own_turn_prompt(&mut self) {
        let Some(message_id) = self
            .own_turn
            .as_ref()
            .map(|anchor| anchor.message_id.clone())
        else {
            return;
        };
        let exists = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id);
        let keep = self
            .own_turn
            .as_mut()
            .is_some_and(|anchor| anchor.observe_prompt(exists));
        if keep {
            return;
        }

        self.own_turn = None;
        self.own_turn_kick = false;
        self.own_turn_last_tick = None;
        self.remeasure_last_row();
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        self.viewport_finalize_pending = true;
    }

    /// Install the reservation before list layout. Its height follows the
    /// current viewport in that same pass, including window resizes.
    fn update_runway_minimum(&mut self) {
        if let Some(ix) = self.own_turn_anchor_ix() {
            self.list.set_tail_reservation(Some((
                ix,
                px(Self::own_send_inset(ix) - OWN_SEND_SCROLL_SLACK_PX),
            )));
        } else if self.own_turn.is_none() {
            self.list.set_tail_reservation(None);
        }
    }

    fn scroll_own_turn_by(&self, delta: f32) {
        let offset = self.list.logical_scroll_top();
        if offset.item_ix == 0 && offset.offset_in_item < px(0.0) {
            self.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: offset.offset_in_item + px(delta),
            });
        } else {
            self.list.scroll_by(px(delta));
        }
    }

    /// Advance the prompt glide or hand a filled reservation to tail-follow.
    /// Reservation sizing happens in the list layout, never in this callback.
    fn step_own_turn(&mut self, cx: &mut Context<Self>) {
        self.own_turn_kick = false;
        // Layout moves the bottom too (pad refinement, streaming growth):
        // refresh the wheel handler's escape baseline every frame so only a
        // WHEEL's own delta registers as user intent. Without this, the pad
        // growing at turn-completion between two wheel events read as
        // "scrolled away" and silently released the hold — the next wheels
        // then sank unopposed deep into the runway blank (rig-traced).
        self.last_scroll_distance = self.distance_from_bottom();
        let Some(anchor_ix) = self.own_turn_anchor_ix() else {
            // The optimistic echo may arrive on the next state notification.
            return;
        };
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.seen_prompt = true;
        }
        let viewport = self.list.viewport_bounds();
        let viewport_height = f32::from(viewport.size.height);
        if viewport_height <= 0.0 {
            self.own_turn_kick = true;
            cx.notify();
            return;
        }
        let inset = Self::own_send_inset(anchor_ix);
        if self.is_glued() && self.own_turn.as_ref().is_some_and(|anchor| anchor.held) {
            self.list.scroll_by(px(-viewport_height));
        }
        let anchor_bounds = self.list.bounds_for_item(anchor_ix);
        // The list consumes the reservation in the same layout that measures
        // new rows. The height tree remains available when the prompt or tail
        // is outside the viewport, so neither can block the handoff.
        if self.list.tail_reservation_filled() {
            let held = self.own_turn.take().is_some_and(|a| a.held);
            self.own_turn_last_tick = None;
            self.list.set_tail_reservation(None);
            if held || self.pinned || self.distance_from_bottom() <= AT_BOTTOM_PX {
                self.engage_pin(cx);
            } else {
                cx.notify();
            }
            return;
        }

        // ---- entry glide, then absolute hold -------------------------------
        let (held, positioned) = self
            .own_turn
            .as_ref()
            .map_or((false, false), |a| (a.held, a.positioned));
        if !held {
            return;
        }
        if positioned {
            // Landed: re-assert the prompt's position after every layout.
            // scroll_to is absolute and bounds-independent, so neither glue
            // re-snaps, pad-sizing lag, nor a splice's unmeasured flicker can
            // carry the view off the prompt (each broke the spring-held
            // variants of this — rig-traced). ONE-SIDED: only upward drift
            // (view above the hold) is corrected. The scroll slack under the
            // reservation is legal resting space — wheel-down sinks into it
            // and stops hard at the list's own clamp; snapping back up from
            // there made the bottom bounce/stutter on every scroll event
            // (user report). Way-below-slack (impossible short of a bug)
            // still re-asserts.
            let moved = match anchor_bounds {
                Some(b) => {
                    let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                    // The legal rest zone below the hold is the epsilon plus
                    // rounding; anything deeper is a transient-collision sink
                    // and rubber-bands back.
                    err > 0.5 || err < -(OWN_SEND_SCROLL_SLACK_PX + 2.0)
                }
                // Bounds vanish in the glued representation (dissolved
                // above, so at most for this one frame) and through splice
                // flicker. Near the stop that is dead-band space — no
                // assert (asserting on None here was the bottom bounce);
                // far from it the position is unknowable flicker: re-assert.
                None => self.distance_from_bottom() > OWN_SEND_SCROLL_SLACK_PX + 8.0,
            };
            if moved {
                // Correct with the entry glide's ease, not a snap: the only
                // in-band escapes are one-frame commit transients and splice
                // flicker, and an eased ~200ms return reads as native
                // rubber-banding where an instant re-assert read as stutter
                // (user report). Bounds-less flicker still snaps — there is
                // nothing to ease against.
                match anchor_bounds {
                    Some(b) => {
                        let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                        let now = Instant::now();
                        let frames = match self.own_turn_last_tick {
                            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0
                                / SPRING_FRAME_MS)
                                .min(SPRING_MAX_CATCHUP_FRAMES),
                            None => 1.0,
                        };
                        self.own_turn_last_tick = Some(now);
                        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
                        if err.abs() <= OWN_SEND_GLIDE_SNAP_PX {
                            self.list.scroll_by(px(err));
                            self.own_turn_last_tick = None;
                        } else {
                            self.scroll_own_turn_by(err * ease);
                        }
                        self.own_turn_kick = true;
                    }
                    None => {
                        self.list.scroll_to(ListOffset {
                            item_ix: anchor_ix,
                            offset_in_item: px(-inset),
                        });
                        self.own_turn_last_tick = None;
                    }
                }
                cx.notify();
            } else {
                self.own_turn_last_tick = None;
            }
            return;
        }
        let now = Instant::now();
        let frames = match self.own_turn_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.own_turn_last_tick = Some(now);
        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
        // Prefer the prompt geometry. When it is being remeasured but is
        // already the scroll anchor, its logical offset is equally exact.
        // Otherwise approach through the unmeasured rows, capping every step
        // at the prompt so the provisional minimum can never cause overshoot.
        let (err, anchored) = match anchor_bounds {
            Some(bounds) => (
                f32::from(bounds.top()) - (f32::from(viewport.top()) + inset),
                true,
            ),
            None if self.list.logical_scroll_top().item_ix == anchor_ix => (
                -f32::from(self.list.logical_scroll_top().offset_in_item) - inset,
                true,
            ),
            None => {
                // Remeasurement retains preceding row heights as hints. Read
                // the prompt's coordinate from that same height tree rather
                // than aiming at the provisional minimum's (larger) bottom.
                let current = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
                let target = -f32::from(self.list.offset_for_item(anchor_ix)) + inset;
                (current - target, false)
            }
        };
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport_height;
        let err = if err > glide_max {
            self.list.scroll_by(px(err - glide_max));
            glide_max
        } else {
            err
        };
        let land = |list: &ListState| {
            list.scroll_to(ListOffset {
                item_ix: anchor_ix,
                offset_in_item: px(-inset),
            });
        };
        if motion::reduced_motion(cx) {
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if anchored
            && err <= OWN_SEND_GLIDE_SNAP_PX
            && err >= -(OWN_SEND_SCROLL_SLACK_PX + 2.0)
        {
            // At the hold — or resting inside the slack under it (a restick
            // that fired at the true bottom): land WITHOUT pulling the view
            // up. Only a still-above position gets the snap.
            if err > 0.5 {
                land(&self.list);
            }
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if !anchored && err <= OWN_SEND_GLIDE_SNAP_PX {
            // The height hints put us at the prompt. Land by row identity
            // so its final measurement cannot leave us in the reservation.
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else {
            self.scroll_own_turn_by(err * ease);
            if own_turn_glide_crossed(self.list.logical_scroll_top(), anchor_ix, inset) {
                land(&self.list);
            }
        }
        self.own_turn_kick = true;
        cx.notify();
    }

    /// Whether the transcript is currently pinned to the bottom.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the shell should float the "Scroll to bottom" pill (scrolled
    /// more than [`SCROLL_BUTTON_THRESHOLD_PX`] off the end, unpinned).
    pub fn jump_button_shown(&self) -> bool {
        self.show_jump_button
    }

    /// The scroll-to-bottom pill's click: glide back to the end and re-pin.
    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.user_collapse_scroll = None;
        self.cancel_user_hold();
        self.discard_pending_viewport();
        // With a live runway, "bottom" IS the held position (the reservation
        // makes prompt-at-top and pad-bottom the same place): re-arm the hold
        // and glide back instead of destroying the runway (user spec — only
        // navigating away and back clears it).
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.held = true;
            anchor.positioned = false;
            self.own_turn_last_tick = None;
            self.own_turn_kick = true;
            self.show_jump_button = false;
            cx.notify();
            return;
        }
        self.engage_pin(cx);
    }

    /// Re-engage the bottom pin with a glide. Long jumps teleport to within
    /// [`GLIDE_MAX_VIEWPORTS`] of the end first (mugen `springToBottom`);
    /// reduced motion snaps.
    fn engage_pin(&mut self, cx: &mut Context<Self>) {
        self.pinned = true;
        self.show_jump_button = false;
        if motion::reduced_motion(cx) {
            self.list.scroll_to_end();
            cx.notify();
            return;
        }
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let distance = self.distance_from_bottom();
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
        }
        self.wake_spring();
        cx.notify();
    }

    /// Arm the per-frame spring driver — `render` schedules the next frame
    /// while [`Self::spring_should_run`].
    fn wake_spring(&mut self) {
        if self.spring_settled_at.is_some_and(|settled| {
            settled.elapsed() >= Duration::from_millis(SPRING_SETTLE_GRACE_MS)
        }) {
            self.spring.reset();
            self.spring_last_tick = None;
        }
        self.spring_settled_at = None;
        self.spring_kick = true;
    }

    /// A layout kick needs one observation; otherwise only unfinished motion
    /// needs another frame. The settle grace retains state without repainting.
    fn spring_should_run(&self) -> bool {
        self.spring_kick || StickSpring::needs_frame(self.distance_from_bottom())
    }

    /// Whether the scroll offset is in a bottom-glued representation (`None`
    /// or anchored past the end) — states where the next layout hard-snaps to
    /// the new end instead of holding a pixel position.
    pub(crate) fn is_glued(&self) -> bool {
        self.list.logical_scroll_top().item_ix >= self.rows.len()
    }

    /// One spring frame: observe target growth, step the stepper, apply the
    /// delta, and park on landing. Runs from `window.on_next_frame`,
    /// i.e. after layout — measurements are fresh.
    fn step_spring(&mut self, cx: &mut Context<Self>) {
        self.spring_kick = false;
        if !self.pinned {
            self.spring_last_tick = None;
            return;
        }
        let now = Instant::now();
        if self.spring_settled_at.is_some_and(|settled| {
            now.duration_since(settled) >= Duration::from_millis(SPRING_SETTLE_GRACE_MS)
        }) {
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
        }
        let frames = match self.spring_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.spring_last_tick = Some(now);

        let target = f32::from(self.list.max_offset_for_scrollbar().y);
        let mut distance = self.distance_from_bottom();
        // Long jumps (chat switch mid-history, huge pastes) teleport first.
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
            distance = glide_max;
        }
        let pos = target - distance;
        let next = self.spring.step(pos, target, frames);
        if next > pos {
            self.list.scroll_by(px(next - pos));
        }
        self.last_scroll_distance = (target - next).max(0.0);

        if target - next <= 0.5 {
            // Land on the final item, not the scrollbar's estimated pixel
            // total. Remeasuring virtual rows can otherwise move that total
            // after every landing and restart the glide indefinitely.
            self.list.scroll_to_end();
            self.spring_settled_at.get_or_insert(now);
        } else {
            self.spring_settled_at = None;
        }
        // A stationary spring used to repaint throughout the 500ms grace.
        // Repeated layout kicks kept that loop alive for entire streams even
        // at distance=0, velocity=0. Preserve the final movement's paint and
        // every moving frame; a settled spring wakes on the next layout kick.
        if next > pos || StickSpring::needs_frame(self.last_scroll_distance) {
            cx.notify();
        }
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, replay) = {
            let s = self.state.read(cx);
            match &self.doc_override {
                // Pinned to a subagent doc: `selected` equals `chat_id` by
                // construction, so the attach/reset branch below never fires,
                // and echoes stay empty (nothing is ever sent from here).
                Some(doc_id) => (Some(doc_id.clone()), TranscriptReplayState::Populated),
                None => {
                    let replay = if !s.transcript_replayed {
                        TranscriptReplayState::Pending
                    } else if s.transcript.is_empty() {
                        TranscriptReplayState::Empty
                    } else {
                        TranscriptReplayState::Populated
                    };
                    (s.selected_chat.clone(), replay)
                }
            }
        };

        let source = (
            selected.clone(),
            replay,
            self.state.read(cx).transcript_revision,
        );
        if self.last_source.as_ref() == Some(&source) {
            return;
        }
        self.last_source = Some(source);

        let attached = selected != self.chat_id;
        if attached {
            // Read the incoming snapshot before inserting the outgoing one:
            // a full bounded cache may evict its oldest entry, which can be
            // exactly the chat the user is reopening.
            let saved_viewport = selected
                .as_ref()
                .and_then(|chat_id| self.saved_viewports.get_cloned_and_touch(chat_id));
            self.remember_current_viewport();
            let keep_own_turn = self
                .own_turn
                .as_ref()
                .is_some_and(|anchor| selected.as_deref() == Some(anchor.chat_id.as_str()));
            if !keep_own_turn {
                self.own_turn = None;
                self.own_turn_kick = false;
                self.own_turn_last_tick = None;
            }
            self.chat_id = selected;
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.user_folds.clear();
            self.user_heights.clear();
            self.user_hold_token = self.user_hold_token.wrapping_add(1);
            self.user_hold_task = None;
            self.user_collapse_scroll = None;
            self.veils.clear();
            self.render_cache.borrow_mut().clear();
            self.highlights.entries.clear();
            self.copied_message = None;
            self.copied_message_clear = None;
            self.list.reset(0);
            self.pending_viewport = None;
            self.viewport_generation = self.viewport_generation.wrapping_add(1);
            self.viewport_finalize_pending = false;
            if self.own_turn.is_some() {
                // A kept own-turn hold (send-created chat) owns the viewport.
                self.pinned = false;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            } else if let Some(SavedViewport::Anchored {
                anchor,
                distance_from_bottom,
                own_turn,
            }) = saved_viewport
            {
                // Keep a possible runway pending until replay confirms that
                // its optimistic prompt still exists. Installing it on this
                // empty attach frame can leave a failed send's stale anchor
                // intercepting scroll-to-bottom forever.
                self.pinned = false;
                self.last_scroll_distance = distance_from_bottom;
                self.show_jump_button = distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
                self.pending_viewport = Some(SavedViewport::Anchored {
                    anchor,
                    distance_from_bottom,
                    own_turn,
                });
            } else {
                // New chats and chats that were following their tail retain
                // the existing open-at-bottom behavior.
                self.pinned = true;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            }
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
            self.spring_kick = false;
            self.scroll_anim = None;
            self.stop_selection_scroll();
        }

        let mut new_rows: Vec<Row> = Vec::new();
        // Borrow the transcript only while deriving rows. Cloning the entity
        // handle lets rows_for mutate our caches without copying every text
        // and tool payload on each app-state notification.
        let (entries_empty, tail_streaming) = {
            let state = self.state.clone();
            let state = state.read(cx);
            let entries = match &self.doc_override {
                Some(doc_id) => state.sub_transcript(doc_id),
                None => state.transcript.as_slice(),
            };
            for entry in entries {
                new_rows.extend(self.rows_for(entry, false));
            }
            if self.doc_override.is_none() {
                for echo in state.pending_echoes() {
                    new_rows.extend(self.rows_for(echo, true));
                }
            }
            (
                entries.is_empty(),
                entries
                    .last()
                    .is_some_and(|e| e.status == Some(MessageStatus::Streaming)),
            )
        };

        // Runtime scroll handles follow the stable code rows exactly. A live
        // block keeps its handle through completion; deleted/reindexed tail
        // blocks and the previous chat cannot accumulate stale handles.
        let active_code_fences: HashSet<SharedString> = new_rows
            .iter()
            .flat_map(|row| match &row.kind {
                RowKind::Markdown { tree, block_ix } | RowKind::LiveMarkdown { tree, block_ix } => {
                    tree.blocks
                        .get(*block_ix)
                        .map(|top| {
                            render::code_block_indices(&top.block, *block_ix)
                                .into_iter()
                                .map(|ix| format!("{}#code{ix}", row.id).into())
                                .collect()
                        })
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            })
            .collect();
        self.code_fences
            .retain(|key, _| active_code_fences.contains(key));

        // Text already streamed before this (re)attach is the veil BASELINE:
        // its rows' veils seed instead of fading (render creates them from
        // this set), so only post-switch appends animate. Captured from the
        // first NON-EMPTY transcript after attach — the replay frame — never
        // the attach-time sync, whose transcript is still empty (selection
        // clears it; the doc watch refills it async).
        if attached {
            self.veil_baseline.clear();
            self.veil_attach_pending = true;
        }
        if self.veil_attach_pending && !entries_empty {
            self.veil_attach_pending = false;
            self.veil_baseline = new_rows
                .iter()
                .filter(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
                .map(|r| r.id.clone())
                .collect();
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→complete flip (any mid-fade chunk snaps to full, matching the
        // row's version splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });
        self.veil_baseline.retain(|id| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        // Capture this before the row splice changes the list's measured end.
        // When the user is truly live-following, retaining the end anchor
        // keeps the in-flow working trailer at the same viewport position as
        // transcript lines grow above it. Nothing about the trailer's layout
        // or coordinates changes.
        let live_following =
            should_anchor_live_stream(self.pinned, self.distance_from_bottom(), tail_streaming);
        let was_empty = self.rows.is_empty();
        let old_last = self.rows.len().checked_sub(1);
        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;
                self.refresh_protected_attachments(cx);
                self.reconcile_own_turn_prompt();
                // Replay readiness is independent of row content: an empty
                // reset (or one identical to optimistic rows) still resolves
                // or retires the pending viewport.
                if self.restore_pending_viewport(replay) {
                    cx.notify();
                }
                return;
            }
            Some((old_range, count)) => {
                // Any replaced row's cached flatten results are stale — and
                // because live replies splice only the rows whose content hash
                // changed (the tail), this is O(changed rows) per commit, never
                // O(reply).
                for row in &self.rows[old_range.clone()] {
                    self.render_cache.borrow_mut().invalidate_row(&row.id);
                }
                if old_range.len() == count {
                    // In-place content change, same row count — notably the
                    // live→complete flip, where EVERY row of the streamed
                    // message changes version (streaming bit, tool auto_open,
                    // timestamp bit) with identical ids. `splice` would reset
                    // those items to hint-less Unmeasured (heights read 0
                    // until the next paint) and, when the viewport-top item is
                    // inside the range, clobber the scroll anchor to the range
                    // start — the end-of-turn up/down jump the spring then has
                    // to walk back. `remeasure_items` keeps old sizes as hints
                    // and holds the anchor across the remeasure.
                    self.list.remeasure_items(old_range);
                } else {
                    self.list.splice(old_range, count);
                }
                self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
            }
        }
        self.rows = new_rows;
        if old_last != self.rows.len().checked_sub(1) {
            if let Some(ix) = old_last.filter(|&ix| ix < self.rows.len()) {
                // Bottom chrome moves to the new tail too.
                self.list.remeasure_items(ix..ix + 1);
            }
            if was_empty && self.own_turn.is_some() && !self.rows.is_empty() {
                // There was no concrete row to materialize at send time.
                // Start the echo at the bottom edge before adding its runway.
                self.list.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: -self.list.viewport_bounds().size.height,
                });
            }
        }
        self.refresh_protected_attachments(cx);
        self.reconcile_own_turn_prompt();
        self.restore_pending_viewport(replay);
        if self.land_end_pending && !self.rows.is_empty() {
            // First content for an unpinned override tab: land at the end.
            // `scroll_to_end` is ITEM-anchored (past-the-end offset that the
            // next layout materializes) — a pixel scroll off `max_offset`
            // would land short here, since the freshly-spliced rows are
            // still unmeasured. Short content clamps back to the top under
            // Top alignment, so "end" and "top" coincide there.
            self.land_end_pending = false;
            self.list.scroll_to_end();
        }
        if self.own_turn.is_some() {
            self.own_turn_kick = true;
        }
        if self.pinned {
            if live_following {
                self.list.scroll_to_end();
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                self.spring_kick = false;
                self.last_scroll_distance = 0.0;
            } else {
                if motion::reduced_motion(cx) || was_empty {
                    // First fill (chat open) lands at the bottom instantly
                    // (mugen initialScroll:'bottom'); reduced motion snaps.
                    self.list.scroll_to_end();
                } else if self.is_glued() {
                    // A glued offset (`None` / anchored past the end) makes
                    // the upcoming layout hard-snap to the new end — the
                    // per-commit stutter. Materialize a pixel anchor a hair
                    // above the bottom so layout holds position and the
                    // spring glides the growth.
                    self.list.scroll_by(px(-0.75));
                }
                self.spring_kick = true;
            }
        }
        cx.notify();
    }

    /// Cached row build for one entry (streaming entries bypass the cache).
    fn rows_for(&mut self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        // Live entries always rebuild; don't allocate a fingerprint that the
        // streaming path cannot use.
        let fingerprint = if streaming {
            0
        } else {
            entry_fingerprint(entry, pending)
        };
        if !streaming
            && let Some(cached) = self.row_cache.get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            // Render-cache invalidation rides on the row diff in `sync` (only
            // rows whose content hash changed are spliced — the reparsed tail).
            parse_for_row(streaming, key, text, live_parsers, tree_cache).0
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

    /// Fetch a sidecar blob (full tool output or diff) and build its upgraded
    /// [`ToolDetail`] once, off the render path. Re-entry while Loading/Ready
    /// is a no-op; Failed re-arms as a retry (the affordance label says so).
    fn spawn_blob_fetch(&mut self, blob_ref: SharedString, cx: &mut Context<Self>) {
        // Rank BEFORE the already-fetched guard: clicking a Ready ref is the
        // "show me this one again" toggle (recency bump + repaint, no
        // re-fetch) — with both a diff and an output fetched, the two
        // affordances must be able to trade places forever.
        self.blob_fetch_counter += 1;
        self.blob_fetch_order
            .insert(blob_ref.clone(), self.blob_fetch_counter);
        match self.blob_details.get(&blob_ref) {
            Some(BlobFetch::Ready(_)) => {
                cx.notify();
                return;
            }
            Some(BlobFetch::Loading(_)) => return,
            Some(BlobFetch::Failed) | None => {}
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let is_diff = blob_ref.ends_with(".diff");
        let ref_key = blob_ref.clone();
        let task = cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                zeron_rpc::methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": ref_key.as_ref() }),
                Duration::from_secs(20),
            )
            .await;
            let fetched = match reply {
                Ok(value) => {
                    let text = value
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    blob_detail(text, is_diff)
                        .map(|d| BlobFetch::Ready(Arc::new(d)))
                        .unwrap_or(BlobFetch::Failed)
                }
                Err(_) => BlobFetch::Failed,
            };
            this.update(cx, |this, cx| {
                this.blob_details.insert(ref_key, fetched);
                cx.notify();
            })
            .ok();
        });
        self.blob_details.insert(blob_ref, BlobFetch::Loading(task));
    }

    /// Expand/collapse one long user bubble. Heights come from the text's
    /// passive paint cache, never from transcript state, so this changes
    /// render-local fold state only and does not rebuild or splice rows.
    fn toggle_user_fold(
        &mut self,
        row_id: SharedString,
        row_ix: usize,
        collapsed_h: f32,
        full_h: f32,
        reduced_motion: bool,
    ) {
        let duration_ms = user_resize_duration_ms(full_h - collapsed_h);
        // A fold owns the viewport just like explicit navigation. Release
        // the sent-turn hold as well as the spring: otherwise growing beyond
        // the reserved space hands the still-held turn back to the bottom
        // pin and hides the beginning of the newly expanded prompt.
        self.begin_scroll_navigation();
        let entry = self.user_folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(false);
        entry.from = if currently_open { full_h } else { collapsed_h };
        entry.open = Some(!currently_open);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
        entry.duration_ms = duration_ms;

        // Capture a screen-space anchor for the clicked row. We do not subtract
        // the removed height: that is only correct when the row's top is
        // exactly at the viewport bottom. At the top or middle of a long
        // message it overscrolls past the collapsed bubble. The same anchor is
        // used for expansion, with the full target height, so a bottom-pinned
        // prompt reveals its beginning below the top fade instead of above it.
        let Some(item_bounds) = self.list.bounds_for_item(row_ix) else {
            return;
        };
        let viewport = self.list.viewport_bounds();
        let initial_top = f32::from(item_bounds.top());
        // Keep a newly revealed bubble below the transcript's top fade band. A
        // 12px inset alone still leaves the first lines washed into the edge
        // fade when the expanded row started above view.
        let viewport_top = f32::from(viewport.top()) + Theme::TRANSCRIPT_FADE_BAND + 28.0;
        let target_height = if currently_open { collapsed_h } else { full_h };
        let viewport_bottom = f32::from(viewport.bottom()) - target_height - 12.0;
        let target_top = if viewport_bottom >= viewport_top {
            initial_top.clamp(viewport_top, viewport_bottom)
        } else {
            viewport_top
        };
        let needs_scroll = (target_top - initial_top).abs() > 0.5;

        if needs_scroll {
            if reduced_motion {
                if let Some(current) = self.list.bounds_for_item(row_ix) {
                    self.list
                        .scroll_by(px(f32::from(current.top()) - target_top));
                }
            } else {
                self.user_collapse_scroll = Some(UserCollapseScroll {
                    started_at: Instant::now(),
                    duration_ms,
                    height_delta: (full_h - collapsed_h).max(0.0),
                    row_ix,
                    initial_top,
                    target_top,
                });
            }
        }
    }

    fn cancel_user_hold(&mut self) {
        self.user_hold_token = self.user_hold_token.wrapping_add(1);
        self.user_hold_task = None;
    }

    /// Arm a long-press toggle instead of using double-click. Releasing before
    /// the threshold preserves an ordinary click/selection gesture; moving
    /// cancels the timer so drag selection never unexpectedly toggles the
    /// message.
    fn arm_user_hold(
        &mut self,
        row_id: SharedString,
        row_ix: usize,
        collapsed_h: f32,
        measured_h: Rc<Cell<f32>>,
        selection_key: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        const USER_HOLD_DELAY: Duration = Duration::from_millis(360);
        self.cancel_user_hold();
        self.user_hold_token = self.user_hold_token.wrapping_add(1);
        let token = self.user_hold_token;
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(USER_HOLD_DELAY).await;
            this.update(cx, |this, cx| {
                if this.user_hold_token != token {
                    return;
                }
                this.user_hold_task = None;
                crate::markdown::selection::clear_if_owner(&selection_key);
                this.toggle_user_fold(
                    row_id,
                    row_ix,
                    collapsed_h,
                    measured_h.get().max(collapsed_h),
                    motion::reduced_motion(cx),
                );
                cx.notify();
            })
            .ok();
        });
        self.user_hold_task = Some(task);
    }

    fn step_user_collapse_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(scroll) = self.user_collapse_scroll.as_ref() else {
            return;
        };
        let started_at = scroll.started_at;
        let duration_ms = scroll.duration_ms;
        let height_delta = scroll.height_delta;
        let row_ix = scroll.row_ix;
        let initial_top = scroll.initial_top;
        let target_top = scroll.target_top;
        let raw =
            (started_at.elapsed().as_secs_f32() / (duration_ms as f32 / 1000.0)).clamp(0.0, 1.0);
        let spec = user_resize_spec(height_delta);
        let progress = spec.progress(raw);
        let desired_top = motion::lerp(initial_top, target_top, progress);
        if let Some(current) = self.list.bounds_for_item(row_ix) {
            // `scroll_by(+x)` moves content up, so correcting current minus
            // desired keeps the row on the interpolated screen-space path.
            let correction = f32::from(current.top()) - desired_top;
            if correction.abs() > 0.1 {
                self.list.scroll_by(px(correction));
            }
        }
        if raw >= 1.0 {
            self.user_collapse_scroll = None;
            self.last_scroll_distance = self.distance_from_bottom();
            self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        }
        cx.notify();
    }

    fn toggle_fold(&mut self, row_id: SharedString, open_height: f32, auto_open: bool) {
        let entry = self.folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(auto_open);
        entry.from = if currently_open { open_height } else { 0.0 };
        entry.open = Some(!currently_open);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
    }

    // ---- attachment read-back (user-attachments.tsx + transcript cache) ----

    /// Shield the open transcript's attachments from image-cache eviction —
    /// rebuilt on every row sync so a chat switch swaps the set. Without it,
    /// budget pressure evicted thumbnails still on screen (the list caches
    /// rendered rows, so a visible image's LRU tick goes stale).
    fn refresh_protected_attachments(&self, cx: &Context<Self>) {
        // The protected set is GLOBAL and replaced wholesale — an override
        // instance writing it would clobber the primary transcript's keys.
        if self.doc_override.is_some() {
            return;
        }
        let devices = self.attachment_device_ids(cx);
        let mut keys = std::collections::HashSet::new();
        for row in &self.rows {
            if let RowKind::User { attachments, .. } = &row.kind {
                for att in attachments.iter() {
                    for dev in &devices {
                        keys.insert((dev.clone(), att.path.clone()));
                    }
                }
            }
        }
        crate::attachments::protect_attachments(keys);
    }

    /// Devices that may own a user message's attachment files: the chat's host
    /// device (uploads targeted it) plus this device (zeron's
    /// `uniqueIds([attachmentDeviceId, m.device_id])`).
    fn attachment_device_ids(&self, cx: &Context<Self>) -> Vec<String> {
        // `selected_chat_row` belongs to the PRIMARY transcript's chat — an
        // override instance has no chat row, so it claims no devices (its
        // thumbnails degrade to placeholders instead of guessing).
        if self.doc_override.is_some() {
            return Vec::new();
        }
        let state = self.state.read(cx);
        let mut ids = Vec::new();
        if let Some(chat) = state.selected_chat_row() {
            ids.push(chat.device_id.clone());
        }
        if let Some(local) = state.local_device_id.clone()
            && !ids.contains(&local)
        {
            ids.push(local);
        }
        ids
    }

    /// Effective load state for one attachment across its candidate devices:
    /// first Loaded source wins; otherwise loads are (re)claimed and the
    /// snapshot degrades Loading → Error with a scheduled retry wake-up.
    fn attachment_state(
        &mut self,
        device_ids: &[String],
        path: &str,
        cx: &mut Context<Self>,
    ) -> crate::attachments::AttachmentSnapshot {
        use crate::attachments::{AttachmentSnapshot, attachment_snapshot, begin_load};
        for dev in device_ids {
            if let AttachmentSnapshot::Loaded(image) = attachment_snapshot(dev, path) {
                return AttachmentSnapshot::Loaded(image);
            }
        }
        let mut any_loading = false;
        let mut min_retry: Option<Duration> = None;
        for dev in device_ids {
            if begin_load(dev, path) {
                self.spawn_attachment_load(dev.clone(), path.to_string(), cx);
            }
            match attachment_snapshot(dev, path) {
                AttachmentSnapshot::Loaded(image) => return AttachmentSnapshot::Loaded(image),
                AttachmentSnapshot::Loading => any_loading = true,
                AttachmentSnapshot::Error { retry_in } => {
                    min_retry = Some(min_retry.map_or(retry_in, |m| m.min(retry_in)));
                }
            }
        }
        if any_loading {
            return AttachmentSnapshot::Loading;
        }
        match min_retry {
            Some(retry_in) => {
                if let Some(dev) = device_ids.first() {
                    self.schedule_attachment_retry((dev.clone(), path.to_string()), retry_in, cx);
                }
                AttachmentSnapshot::Error { retry_in }
            }
            // No candidate devices at all — the "unavailable" thumb, no retry.
            None => AttachmentSnapshot::Error {
                retry_in: Duration::MAX,
            },
        }
    }

    fn spawn_attachment_load(&mut self, device_id: String, path: String, cx: &mut Context<Self>) {
        use crate::attachments::{read_attachment_image, store_error, store_loaded};
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            store_error(&device_id, &path);
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        // Relay-forward only for a genuinely remote owner; the local device's
        // files are served directly.
        let target = (local.as_deref() != Some(device_id.as_str())).then(|| device_id.clone());
        let key = (device_id.clone(), path.clone());
        let task = cx.spawn(async move |this, cx| {
            match read_attachment_image(&engine, cx.background_executor(), target.as_deref(), &path)
                .await
            {
                Some(loaded) => store_loaded(&device_id, &path, loaded.name.into(), loaded.image),
                None => store_error(&device_id, &path),
            }
            this.update(cx, |transcript, cx| {
                transcript
                    .attachment_loads
                    .remove(&(device_id.clone(), path.clone()));
                cx.notify();
            })
            .ok();
        });
        self.attachment_loads.insert(key, task);
    }

    /// One wake-up per errored source: after the backoff elapses, a notify
    /// re-renders the thumb, whose `begin_load` then claims the retry.
    fn schedule_attachment_retry(
        &mut self,
        key: (String, String),
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if delay == Duration::MAX || self.attachment_retries.contains_key(&key) {
            return;
        }
        let wake = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(delay + Duration::from_millis(60))
                .await;
            this.update(cx, |transcript, cx| {
                transcript.attachment_retries.remove(&wake);
                cx.notify();
            })
            .ok();
        });
        self.attachment_retries.insert(key, task);
    }

    /// The inside of a user bubble: the prompt text, clipped to
    /// [`USER_COLLAPSED_LINES`] until expanded, plus the expander chevron for
    /// prompts past the cap. Returns the bubble's children in order.
    ///
    /// The collapsed form clips a normally-laid-out text element at exactly
    /// five line boxes. Do not use gpui's `line_clamp` here: on an auto-width
    /// flex item it answers intrinsic-width probes with the truncated layout,
    /// collapsing the bubble to min-content width (one character per line).
    /// A plain height clip preserves the original bubble width calculation and
    /// never feeds measured layout back into the virtualized list.
    fn render_user_body(
        &mut self,
        row_id: &SharedString,
        row_ix: usize,
        text: SharedString,
        mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.user_folds.get(row_id).copied().unwrap_or_default();
        let expanded = fold.open.unwrap_or(false);
        let line_height =
            f32::from(crate::typography::ui_rems(USER_LINE_HEIGHT).to_pixels(window.rem_size()));
        let collapsed_text_h = USER_COLLAPSED_LINES as f32 * line_height;
        // Include the continuation line in the resize endpoints so removing
        // it on expansion does not make the bubble jump by a line.
        let collapsed_h = collapsed_text_h + line_height;
        let measured_h = self
            .user_heights
            .entry(row_id.clone())
            .or_insert_with(|| Rc::new(Cell::new(0.0)))
            .clone();
        let measured = measured_h.get();
        let collapsible = text.lines().count() > USER_COLLAPSED_LINES
            || (measured > 0.0 && measured > collapsed_text_h + 0.5)
            || (measured == 0.0 && user_message_needs_collapse(&text));
        let full_h = measured_h.get().max(collapsed_h);

        let hold_key = row_id.clone();
        let hold_height = measured_h.clone();
        let hold_selection: Arc<str> = format!("{row_id}:u").into();
        let body = div()
            .id(SharedString::from(format!("{row_id}-body")))
            // A long press toggles instead of double-click. A normal release
            // remains available for text selection, and pointer movement
            // cancels the pending toggle before a drag can select text.
            .when(collapsible, |el| {
                let down_key = hold_key.clone();
                let down_height = hold_height.clone();
                let down_selection = hold_selection.clone();
                el.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.arm_user_hold(
                            down_key.clone(),
                            row_ix,
                            collapsed_h,
                            down_height.clone(),
                            down_selection.clone(),
                            cx,
                        );
                    }),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, _| {
                        this.cancel_user_hold();
                    }),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, _| {
                        this.cancel_user_hold();
                    }),
                )
                .on_mouse_move(cx.listener(|this, _, _, _| {
                    this.cancel_user_hold();
                }))
            })
            .child(user_bubble_text(
                row_id,
                text,
                mentions,
                theme,
                measured_h.clone(),
                cx.entity_id(),
            ));
        // Height motion uses the same ease-out curve as sidebars, tool folds,
        // and pane transitions, with duration scaled to travel distance. The
        // full text remains laid out behind the clip; only the viewport over it
        // changes, so glyph wrapping never shifts.
        let duration_ms = fold
            .duration_ms
            .max(user_resize_duration_ms(full_h - collapsed_h));
        let animating = collapsible
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < Duration::from_millis(duration_ms + 200))
            && !motion::reduced_motion(cx);
        let ellipsis = || div().h(px(line_height)).child("...");
        let body: AnyElement = if animating {
            let from = fold.from;
            let to = if expanded { full_h } else { collapsed_h };
            let resize = user_resize_spec(full_h - collapsed_h);
            let ellipsis_h = if expanded { 0.0 } else { line_height };
            div()
                .child(div().overflow_hidden().child(body).with_animation(
                    SharedString::from(format!("{row_id}-user-resize-{}", fold.epoch)),
                    resize.animation(),
                    move |el, t| el.h(px((motion::lerp(from, to, t) - ellipsis_h).max(0.0))),
                ))
                .when(!expanded, |el| el.child(ellipsis()))
                .into_any_element()
        } else if collapsible && !expanded {
            div()
                .child(div().h(px(collapsed_text_h)).overflow_hidden().child(body))
                .child(ellipsis())
                .into_any_element()
        } else {
            body.into_any_element()
        };
        div()
            .relative()
            .child(body)
            .when(collapsible, |el| {
                el.child(self.render_user_expander(
                    row_id,
                    row_ix,
                    expanded,
                    collapsed_h,
                    measured_h,
                    theme,
                    cx,
                ))
            })
            .into_any_element()
    }

    /// A plain text link aligned with the message's left edge, following the
    /// continuation ellipsis when collapsed. No pill, border, or button wash.
    fn render_user_expander(
        &mut self,
        row_id: &SharedString,
        row_ix: usize,
        expanded: bool,
        collapsed_h: f32,
        measured_h: Rc<Cell<f32>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let toggle_key = row_id.clone();
        let glyph = if expanded {
            crate::icons::ALT_ARROW_UP
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        let label = if expanded { "Show less" } else { "Show more" };
        let button = div()
            .id(SharedString::from(format!("{row_id}-expander")))
            .group("user-message-toggle")
            .role(gpui::Role::Button)
            .aria_label(if expanded {
                "Collapse message"
            } else {
                "Expand message"
            })
            .aria_expanded(expanded)
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(crate::typography::ui_rems(14.0))
            .line_height(crate::typography::ui_rems(USER_LINE_HEIGHT))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.text_color(theme.text))
            .child(label)
            .child(
                crate::icons::icon(glyph)
                    .size(px(12.0))
                    .text_color(theme.text_muted)
                    .group_hover("user-message-toggle", |s| s.text_color(theme.text)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_user_fold(
                    toggle_key.clone(),
                    row_ix,
                    collapsed_h,
                    measured_h.get().max(collapsed_h),
                    motion::reduced_motion(cx),
                );
                cx.notify();
            }));
        div()
            .mt(px(USER_TOGGLE_GAP))
            .flex()
            .items_start()
            .child(button)
            .into_any_element()
    }

    /// The right-aligned thumbnail strip above a user bubble.
    fn render_user_attachments(
        &mut self,
        row_id: &SharedString,
        atts: &[crate::attachments::UserImageAttachment],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::attachments::AttachmentSnapshot;
        let glyph = Theme::of(cx).glyph;
        let device_ids = self.attachment_device_ids(cx);
        let mut strip = div()
            .w_full()
            .h(px(ATT_STRIP_H))
            .flex()
            .flex_row()
            .justify_end()
            .items_start()
            .gap(px(8.0))
            .overflow_hidden()
            .px(px(4.0))
            .pt(px(4.0));
        for (aix, att) in atts.iter().enumerate() {
            let state = self.attachment_state(&device_ids, &att.path, cx);
            // The in-flight send's progress belongs ON the thumbnail
            // (2026-08-18 user request). Two ref shapes mean "still
            // crossing": the queued flow's `pending://` (bytes ship
            // engine-side after the send; the host rewrites the ref to an
            // absolute path once they land and the run starts) and the
            // legacy echo's synthetic `pending/`. Percent sources, in order:
            // this attachment's own relay transfer (`WatchTransfers`, by the
            // uploadId its ref names — the leg that actually takes time),
            // else the send-wide staging/legacy upload percent. Neither → the
            // indeterminate spinner (staged-but-waiting, retry backoff, or
            // committed-awaiting-rewrite), so the ring never shows a number
            // that isn't a real transfer position (2026-08-20 report: the
            // staging-only percent blinked out in ~100ms and lied about the
            // slow part).
            let sending = att.path.starts_with("pending://") || att.path.starts_with("pending/");
            let upload_id = att
                .path
                .strip_prefix("pending://")
                .and_then(|rest| rest.split_once('/'))
                .map(|(id, _)| id);
            let uploading = upload_id
                .and_then(|id| self.state.read(cx).transfer_percent(id))
                .or_else(|| {
                    sending
                        .then(|| self.state.read(cx).upload_progress_percent())
                        .flatten()
                });
            let frame = div()
                .flex_none()
                .w(px(ATT_THUMB_W))
                .h(px(ATT_THUMB_H))
                .rounded(px(8.0))
                .overflow_hidden();
            let thumb: AnyElement = match state {
                AttachmentSnapshot::Loaded(image) => {
                    let preview = crate::attachments::PreviewImage {
                        name: image.name.clone(),
                        image: image.image.clone(),
                    };
                    frame
                        .id(SharedString::from(format!("{row_id}#att{aix}")))
                        .relative()
                        .border_1()
                        .border_color(crate::theme::hairline(0.11))
                        .bg(crate::theme::ink(0.035))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.attachment_preview = Some(preview.clone());
                            window.focus(&this.attachment_preview_focus, cx);
                            cx.notify();
                        }))
                        .child(
                            img(image.image.clone())
                                // EXPLICIT dims, not size_full: img layout
                                // honors the intrinsic aspect ratio over a
                                // percent height (gpui f8d8a90 repoint), so
                                // size_full let a tall photo grow past the
                                // frame and the rectangular overflow clip
                                // squared the bottom corners (2026-08-19).
                                .w(px(ATT_THUMB_W - 2.0))
                                .h(px(ATT_THUMB_H - 2.0))
                                // The IMG needs its own radii: the frame's
                                // rounding only clips rectangularly, so the
                                // sprite must round its own corners (7 = the
                                // frame's 8 minus its 1px border).
                                .rounded(px(7.0))
                                .object_fit(ObjectFit::Cover),
                        )
                        .when(sending, |el| {
                            // The pulse read registers this entity for frames,
                            // so the overlay stays live even once the trailer's
                            // 30s pending-send bridge has lapsed.
                            let pulse = motion::pulse_wave(motion::pulse_delta(
                                &motion::ZERON_PULSE,
                                cx.entity_id(),
                                cx,
                            ));
                            let indicator: AnyElement = match uploading {
                                Some(pct) => crate::loaders::upload_progress_ring(pct, 34.0),
                                None => crate::loaders::mini_glyph_spinner(
                                    format!("att-sending-{row_id}-{aix}"),
                                    3.0,
                                    glyph,
                                    cx.entity_id(),
                                    cx,
                                )
                                .into_any_element(),
                            };
                            el.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(7.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.38 + 0.05 * pulse))
                                    .child(indicator),
                            )
                        })
                        .into_any_element()
                }
                // Errored/unavailable: the dashed "missing" thumb.
                AttachmentSnapshot::Error { .. } => frame
                    .border_1()
                    .border_dashed()
                    .border_color(crate::theme::hairline(0.14))
                    .bg(crate::theme::ink(0.025))
                    .into_any_element(),
                // Loading: the pulsing skeleton (same wash as popover skeletons).
                AttachmentSnapshot::Loading => frame
                    .border_1()
                    .border_color(crate::theme::hairline(0.08))
                    .bg(crate::theme::ink(0.055))
                    .opacity(
                        0.35 + 0.4
                            * motion::pulse_wave(motion::pulse_delta(
                                &motion::ZERON_PULSE,
                                cx.entity_id(),
                                cx,
                            )),
                    )
                    .into_any_element(),
            };
            strip = strip.child(thumb);
        }
        strip.into_any_element()
    }

    // ---- rendering ----

    /// The working loader, INSIDE the conversation flow: appended under the
    /// last row while the run is live (moved out of the shell's status strip
    /// — user request), so it reads as part of the streaming reply and
    /// scrolls away with it. The spinner drives this entity's frames, which
    /// keeps the elapsed timer ticking through delta-quiet tool runs.
    /// The failed-send retry (trailer affordance): re-kick every delivery
    /// road engine-side (fresh chat2 socket, host nudge, delivery escorts)
    /// and restart the grace clock so the trailer returns to Sending/Queued
    /// while the retry runs.
    fn retry_send(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };
        let engine = self.state.read(cx).engine().cloned();
        self.state.update(cx, |s, cx| {
            s.retry_pending_send(&chat_id, chrono::Utc::now());
            cx.notify();
        });
        if let Some(engine) = engine {
            cx.spawn(async move |_, _| {
                let params = serde_json::json!({ "chatId": chat_id });
                if let Err(err) = engine
                    .client()
                    .call(zeron_rpc::methods::RETRY_DELIVERY, params)
                    .await
                {
                    tracing::warn!(error = %err, "delivery retry RPC failed");
                }
            })
            .detach();
        }
    }

    fn render_working_trailer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = chrono::Utc::now();
        let (sending, queued, elapsed_secs, seed) = if let Some(doc_id) = &self.doc_override {
            // A subagent doc has no Session row — `indicator_for` would read
            // the PARENT chat's live state into this tab. Liveness rides the
            // doc itself instead: the sink's assistant entry streams until
            // the subagent settles (run teardown finalizes abandoned sinks),
            // and a trailing USER entry is a steer still awaiting its reply
            // segment. Frozen snapshots never spin, whatever they claim.
            if !self.doc_live {
                return None;
            }
            let state = self.state.read(cx);
            let last = state.sub_transcript(doc_id).last()?;
            let live =
                last.status == Some(MessageStatus::Streaming) || last.role == MessageRole::User;
            if !live {
                return None;
            }
            let elapsed = ((now.timestamp_millis() - last.created_at).max(0) / 1000) as i64;
            (false, false, elapsed, flavour_seed(doc_id))
        } else {
            let chat_id = self.chat_id.clone()?;
            // Failed-send state first: past the grace window the trailer IS
            // the retry affordance, whatever the indicator fell back to.
            if self.state.read(cx).send_undelivered(&chat_id, now) {
                let theme = Theme::of(cx).clone();
                return Some(
                    div()
                        .id("undelivered-retry")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .pt(px(Theme::SPACE_LG))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.retry_send(cx)))
                        .child(SharedString::from("Not delivered — click to retry"))
                        .into_any_element(),
                );
            }
            let (sending, queued, elapsed) = {
                let state = self.state.read(cx);
                if state.indicator_for(&chat_id, now) != crate::state::Indicator::Working {
                    return None;
                }
                // During the send→turn window the session row's `started_at`
                // still belongs to the PREVIOUS turn — a timer based on the
                // send counted the round-trip and then restarted when the
                // turn actually began (user report). Bridge it as "Sending…"
                // with no timer instead; the word + timer start with the
                // turn.
                let turn_started = state.session_for(&chat_id).and_then(|s| s.started_at);
                let sending =
                    sending_bridge(state.pending_send_started(&chat_id, now), turn_started);
                // Degraded delivery path: the send is a durable local write
                // waiting on connectivity — say so instead of faking
                // progress. (The overlay holds while degraded, so this line
                // owns the surface until the ack or the failed state.)
                let queued = sending && state.chat_delivery_degraded(&chat_id);
                let elapsed = turn_started
                    .map(|t| now.signed_duration_since(t).num_seconds().max(0))
                    .unwrap_or(0);
                (sending, queued, elapsed)
            };
            (sending, queued, elapsed, flavour_seed(&chat_id))
        };
        let word = if queued {
            "Queued — will send automatically"
        } else if sending {
            "Sending"
        } else {
            flavour_word(seed, elapsed_secs)
        };
        let theme = Theme::of(cx).clone();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .pt(px(Theme::SPACE_LG))
                .text_size(crate::typography::ui_rems(11.0))
                .child(crate::loaders::gradient_spinner(
                    "working-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(if queued {
                            theme.warning
                        } else {
                            theme.text_muted
                        })
                        .child(SharedString::from(if queued {
                            word.to_string()
                        } else {
                            format!("{word}…")
                        })),
                )
                .when(!sending, |el| {
                    el.child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format_elapsed(elapsed_secs))),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        self.rendered_rows.insert(row.id.clone());
        let theme = Theme::of(cx).clone();
        // The viewport spans the full window (under the titlebar): the first
        // row's gap adds the titlebar's height so a top-scrolled transcript
        // rests below the chrome it fades under. The right pane already pads
        // for the titlebar — an override instance's first row keeps only the
        // ordinary turn gap, or the content sits double-chrome low.
        let top_gap = if ix == 0 {
            if self.doc_override.is_some() {
                Theme::SPACE_LG
            } else {
                Theme::TITLEBAR_HEIGHT + Theme::SPACE_LG + 10.0
            }
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
        };
        // The last row must clear the composer/status stack the transcript
        // scrolls under PLUS the fade band above it, or the timestamp strip
        // (the row's lowest content) renders half-faded (or hidden) when the
        // transcript is pinned to the bottom.
        let is_last = ix + 1 == self.rows.len();
        let bottom_pad = if is_last {
            self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0
        } else {
            0.0
        };
        // Live-run loader rides under the LAST row's content (above its
        // clearance pad), so it sits right beneath the working reply.
        let trailer = (ix + 1 == self.rows.len())
            .then(|| self.render_working_trailer(cx))
            .flatten();

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                text,
                mentions,
                attachments,
                badges,
                pending,
            } => {
                let attachments = attachments.clone();
                let badges = badges.clone();
                let text = text.clone();
                let mentions = mentions.clone();
                let pending = *pending;
                // Attachment thumbnails ride ABOVE the bubble, right-aligned
                // (chat-view.tsx RowView: UserAttachmentStrip then the text
                // HStack); image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
                if !attachments.is_empty() {
                    column = column.child(self.render_user_attachments(&row.id, &attachments, cx));
                }
                if !badges.is_empty() {
                    column = column.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap(px(6.0))
                            .pb(px(6.0))
                            .children(badges.iter().enumerate().map(|(bix, badge)| {
                                crate::badges::render(
                                    SharedString::from(format!("{}#badge{bix}", row.id)),
                                    badge,
                                    &theme,
                                )
                            })),
                    );
                }
                if !text.is_empty() {
                    // `min_w_0` is load-bearing: gpui text answers min/max-content
                    // probes with its UNWRAPPED width, so without it the bubble's
                    // automatic min-size is the full single-line width — the flex
                    // item can't shrink, `justify_end` pushes the overflow off the
                    // left edge, and long prompts render as one clipped line
                    // instead of wrapping inside the 80% column cap.
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                                .bg(crate::theme::user_bubble_bg())
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_size(crate::typography::ui_rems(14.0))
                                .line_height(crate::typography::ui_rems(USER_LINE_HEIGHT))
                                .text_color(theme.text)
                                .when(pending, |el| el.opacity(0.65))
                                .child(self.render_user_body(
                                    &row.id, ix, text, mentions, &theme, window, cx,
                                )),
                        ),
                    );
                }
                column.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let code = self.code_uis_for(&row.id, &top.block, *block_ix, cx);
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: None,
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    code,
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                )
            }
            RowKind::LiveMarkdown { tree, block_ix } => {
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let code = self.code_uis_for(&row.id, &top.block, *block_ix, cx);
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                // Baseline rows (text already streamed when the transcript
                // attached) start seeded: the existing reply must not fade in
                // on a session switch — only fresh appends animate.
                let veil = (!motion::reduced_motion(cx)).then(|| {
                    self.veils
                        .entry(row.id.clone())
                        .or_insert_with(|| {
                            if self.veil_baseline.contains(&row.id) {
                                Rc::new(RefCell::new(RowVeil::seeded()))
                            } else {
                                Rc::default()
                            }
                        })
                        .clone()
                });
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    code,
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let timer = frame_stats_enabled().then(Instant::now);
                let el = render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                );
                if let Some(start) = timer {
                    record_live_frame_us(start.elapsed().as_micros() as u64);
                }
                // The attach pass for this row is done (every element rendered
                // above seeded its baseline synchronously): elements appearing
                // from the NEXT pass on are newly streamed and fade normally.
                if let Some(veil) = &veil {
                    veil.borrow_mut().finish_seeding();
                }
                // Share the loaders' bounded clock. A display-frame callback
                // here would pin the transcript to 60/120Hz for the whole
                // stream, bypassing the clock even with no loader mounted.
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    motion::pulse_lease(cx.entity_id(), cx);
                }
                el
            }
            RowKind::ToolGroup { tools, auto_open } => {
                self.render_tool_group(&row.id, tools, *auto_open, &theme, cx)
            }
            RowKind::InputChip { header, resolved } => {
                input_chip(header.clone(), *resolved, &theme)
            }
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
        };

        // Hover-revealed metadata strip: a RESERVED 32px lane under the
        // entry's last row. Timestamp, copy action, and copied feedback only
        // flip visibility/content, so none of them shifts the virtualizer.
        // User entries align end (under the bubble), assistant entries start.
        // Both read timestamp first, then the copy action.
        let is_user_row = matches!(row.kind, RowKind::User { .. });
        let hovered = self
            .hovered_entry
            .as_ref()
            .is_some_and(|(_, entry)| entry == &row.entry_id);
        let copied_message = self.copied_message.as_ref() == Some(&row.entry_id);
        let copy_text = row.copy_text.clone();
        let copy_entry_id = row.entry_id.clone();
        let strip = row.timestamp.map(|ms| {
            let timestamp = div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted.opacity(0.55))
                .child(SharedString::from(format_timestamp(ms, &chrono::Local)));
            let copy = copy_text.map(|text| {
                let entry_id = copy_entry_id.clone();
                let fade_key = format!("copy-message-hover-{entry_id}");
                div()
                    .id(SharedString::from(format!("copy-message-{entry_id}")))
                    .size(px(Theme::SPACE_MD * 2.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .cursor_pointer()
                    // Same quiet icon-button treatment as the copy action
                    // over transcript code blocks.
                    .bg(motion::hover_blend(
                        &fade_key,
                        gpui::transparent_black(),
                        crate::theme::ink(0.08),
                    ))
                    .on_hover(motion::hover_listener(fade_key))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_message(entry_id.clone(), text.clone(), cx)
                    }))
                    .child(
                        crate::icons::icon(if copied_message {
                            crate::icons::CHECK
                        } else {
                            crate::icons::COPY
                        })
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                    )
            });
            let metadata = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM));
            let metadata = metadata.child(timestamp).children(copy);
            div()
                .h(px(Theme::SPACE_SM + Theme::SPACE_MD * 2.0))
                .pt(px(Theme::SPACE_SM))
                .w_full()
                .flex()
                .items_center()
                // No horizontal inset: the original's `px-1` netted out flush
                // because its message text was inset by the same amount (group
                // padding 4 + inner VStack 4 = 8 = group 4 + px-1 4). Here the
                // markdown text / user bubble sit AT the content column edges,
                // so the label must too — assistant label's left edge on the
                // text's first-character x, user label's right edge on the
                // bubble's right edge (user-reported 4px drift).
                .when(is_user_row, |el| el.justify_end())
                .when(hovered, |el| {
                    el.child(motion::fade_quick(
                        SharedString::from(format!("meta-{}", row.id)),
                        metadata,
                    ))
                })
        });
        let entry_id = row.entry_id.clone();
        let row_id = row.id.clone();
        div()
            .id(row.id.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    let next = Some((row_id.clone(), entry_id.clone()));
                    if this.hovered_entry != next {
                        let entry_changed = this
                            .hovered_entry
                            .as_ref()
                            .is_none_or(|(_, entry)| entry != &entry_id);
                        this.hovered_entry = next;
                        if entry_changed {
                            cx.notify();
                        }
                    }
                } else if this
                    .hovered_entry
                    .as_ref()
                    .is_some_and(|(row, _)| row == &row_id)
                {
                    // Only the row that OWNS the current reveal may clear it —
                    // a stale leave from an earlier row must not blank the
                    // strip the newly entered row just lit.
                    this.hovered_entry = None;
                    cx.notify();
                }
            }))
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (zeron `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .child(inner)
                    .children(strip)
                    .children(trailer),
            )
            .into_any_element()
    }

    fn copy_message(&mut self, entry_id: SharedString, text: SharedString, cx: &mut Context<Self>) {
        cx.stop_propagation();
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.copied_message = Some(entry_id);
        self.copied_message_clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            this.update(cx, |this, cx| {
                this.copied_message = None;
                this.copied_message_clear = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Copy-button wiring for one row's code blocks ([`render::CopyUi`]):
    /// click writes the block's code to the clipboard and shows a transient
    /// "Copied" check on that block for ~1.2s (overlay — no layout shift).
    fn copy_ui_for(&self, row_id: &SharedString, cx: &mut Context<Self>) -> render::CopyUi {
        let copied_ix = self
            .copied_code
            .as_ref()
            .filter(|(id, _)| id == row_id)
            .map(|(_, ix)| *ix);
        let row_key = row_id.clone();
        let entity = cx.weak_entity();
        let handler: Rc<dyn Fn(usize, SharedString, &mut Window, &mut gpui::App)> =
            Rc::new(move |ix, code, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                let row_key = row_key.clone();
                entity
                    .update(cx, |this, cx| {
                        this.copied_code = Some((row_key, ix));
                        this.copied_clear = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(1200))
                                .await;
                            this.update(cx, |this, cx| {
                                this.copied_code = None;
                                this.copied_clear = None;
                                cx.notify();
                            })
                            .ok();
                        }));
                        cx.notify();
                    })
                    .ok();
            });
        render::CopyUi { handler, copied_ix }
    }

    /// Interactive layout/scroll plumbing for one agent Markdown fence. The
    /// persisted Fit choice is global, while each block retains only its own
    /// ephemeral horizontal offset and hover/drag state.
    fn code_ui_for(
        &mut self,
        row_id: &SharedString,
        block_ix: usize,
        cx: &mut Context<Self>,
    ) -> render::CodeUi {
        let key: SharedString = format!("{row_id}#code{block_ix}").into();
        let runtime = self.code_fences.entry(key.clone()).or_default();
        let scroll = runtime.scroll.clone();
        let fit_content = crate::settings::current(cx).code_fences_fit_content;
        let scrollbar = (!fit_content)
            .then(|| runtime.scrollbar.metrics(&scroll))
            .flatten()
            .filter(|_| runtime.scrollbar.visible())
            .map(|metrics| render::CodeScrollbarUi {
                metrics,
                active: runtime.scrollbar.active(),
                hover: {
                    let entity = cx.weak_entity();
                    let key = key.clone();
                    Rc::new(move |hovered, _window, cx| {
                        entity
                            .update(cx, |this, cx| {
                                let Some(runtime) = this.code_fences.get_mut(&key) else {
                                    return;
                                };
                                if runtime.scrollbar.set_bar_hovered(hovered) {
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                },
                press: {
                    let entity = cx.weak_entity();
                    let key = key.clone();
                    Rc::new(move |pointer_x, _window, cx| {
                        entity
                            .update(cx, |this, cx| {
                                let Some(runtime) = this.code_fences.get_mut(&key) else {
                                    return;
                                };
                                let scroll = runtime.scroll.clone();
                                if runtime.scrollbar.begin_press(&scroll, pointer_x) {
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                },
                release: {
                    let entity = cx.weak_entity();
                    let key = key.clone();
                    Rc::new(move |_window, cx| {
                        entity
                            .update(cx, |this, cx| {
                                let Some(runtime) = this.code_fences.get_mut(&key) else {
                                    return;
                                };
                                runtime.scrollbar.end_press();
                                cx.notify();
                            })
                            .ok();
                    })
                },
            });

        render::CodeUi {
            key: key.clone(),
            fit_content,
            scroll,
            scrollbar,
            toggle_fit: {
                Rc::new(move |_window, cx| {
                    let fit = !crate::settings::current(cx).code_fences_fit_content;
                    crate::settings::update(
                        crate::settings::SavePolicy::Immediate,
                        cx,
                        |settings| settings.code_fences_fit_content = fit,
                    );
                    // Every Transcript observes the generation change during
                    // its next render and resets its own local runtime state.
                    cx.refresh_windows();
                })
            },
            viewport_hover: {
                let entity = cx.weak_entity();
                let key = key.clone();
                Rc::new(move |hovered, _window, cx| {
                    entity
                        .update(cx, |this, cx| {
                            let Some(runtime) = this.code_fences.get_mut(&key) else {
                                return;
                            };
                            if runtime.scrollbar.set_viewport_hovered(hovered) {
                                cx.notify();
                            }
                        })
                        .ok();
                })
            },
            drag_move: {
                let entity = cx.weak_entity();
                Rc::new(move |pointer_x, _window, cx| {
                    entity
                        .update(cx, |this, cx| {
                            let Some(runtime) = this.code_fences.get_mut(&key) else {
                                return;
                            };
                            let scroll = runtime.scroll.clone();
                            if runtime.scrollbar.drag_to(&scroll, pointer_x) {
                                cx.notify();
                            }
                        })
                        .ok();
                })
            },
        }
    }

    /// Provision independent interaction state for every fence nested below
    /// one virtualized Markdown row (top-level, quoted, or listed).
    fn code_uis_for(
        &mut self,
        row_id: &SharedString,
        block: &Block,
        block_ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<HashMap<usize, render::CodeUi>> {
        let indices = render::code_block_indices(block, block_ix);
        (!indices.is_empty()).then(|| {
            indices
                .into_iter()
                .map(|ix| (ix, self.code_ui_for(row_id, ix, cx)))
                .collect()
        })
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<zeron_syntax::HighlightedDocument>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language
                    .as_deref()
                    .and_then(zeron_syntax::language_for_alias)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    fn tool_diff_highlight_for(
        &mut self,
        row_id: &SharedString,
        tool_ix: usize,
        detail: &ToolDetail,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::changes::DiffHighlights>> {
        let ToolDetail::Diff {
            file,
            old_text,
            new_text,
        } = detail
        else {
            return None;
        };
        let cache_row: SharedString = format!("{row_id}#tool-diff-{tool_ix}").into();
        let old = match old_text {
            Some(source) => {
                let path = file.old_path.as_deref().unwrap_or(&file.path);
                let lang = zeron_syntax::language_for_path(path)?;
                Some(
                    self.highlights
                        .request(cache_row.clone(), 0, lang, source, cx)?,
                )
            }
            None => None,
        };
        let new = match new_text {
            Some(source) => {
                let lang = zeron_syntax::language_for_path(&file.path)?;
                Some(self.highlights.request(cache_row, 1, lang, source, cx)?)
            }
            None => None,
        };
        Some(Arc::new(crate::changes::DiffHighlights { old, new }))
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
        // Agent/spawn chips never fold: they are their own row, always open,
        // no "Called N tools" header — a running subagent stays visible.
        let collapses = tool_group_collapses(tools);
        let open = !collapses || fold.open.unwrap_or(auto_open);
        // Chips render their EFFECTIVE detail: the precomputed doc-resident
        // one, upgraded in place by a fetched sidecar blob (chat2-sync A3).
        // Resolved per paint (a HashMap probe per chip) so fetched content
        // needs no row rebuild — arrival is a cx.notify, like a fold toggle.
        let details: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| {
                // Spawn chips never expand — the subagent doc is the record
                // of what the tool did, and an inline body would only repeat
                // it. The whole chip is the "open that doc" click instead.
                if is_spawn_link(tool) {
                    return None;
                }
                // Among fetched blobs, the most recently REQUESTED one wins —
                // a tool can carry both a diff and an output ref, and the
                // user's last click decides which upgrade is showing.
                let mut best: Option<(u64, Arc<ToolDetail>)> = None;
                for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                    if let Some(BlobFetch::Ready(detail)) = self.blob_details.get(blob_ref) {
                        let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                        if best.as_ref().is_none_or(|(o, _)| order > *o) {
                            best = Some((order, detail.clone()));
                        }
                    }
                }
                best.map(|(_, d)| d).or_else(|| tool.detail.clone())
            })
            .collect();
        // Full-invocation blocks — with them, EVERY chip expands: the click
        // always answers "what exactly was this call?", output or not.
        let invocations: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| tool.invocation.clone().filter(|_| !is_spawn_link(tool)))
            .collect();
        // Fetch affordance under each open detail whose full payload is still
        // sidecar-only: `(ref, label)`. Diff offered first (the richer
        // upgrade), then the output — a fetched ref hands the affordance to
        // the NEXT unfetched one instead of retiring it (both must stay
        // reachable when a tool has both).
        let affordances: Vec<Option<ChipAffordance>> = tools
            .iter()
            .map(|tool| {
                // The currently-displayed ref (same recency rule as
                // `details` above): its affordance is spent; any OTHER
                // Ready ref stays offered as a no-fetch toggle.
                let shown: Option<&SharedString> = {
                    let mut best: Option<(u64, &SharedString)> = None;
                    for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                        if matches!(self.blob_details.get(blob_ref), Some(BlobFetch::Ready(_))) {
                            let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                            if best.is_none_or(|(o, _)| order > o) {
                                best = Some((order, blob_ref));
                            }
                        }
                    }
                    best.map(|(_, r)| r)
                };
                let candidates = [
                    (tool.diff_ref.as_ref(), "diff", None),
                    (tool.output_ref.as_ref(), "output", tool.output_bytes),
                ];
                for (blob_ref, what, bytes) in candidates {
                    let Some(blob_ref) = blob_ref else { continue };
                    let label = match self.blob_details.get(blob_ref) {
                        Some(BlobFetch::Ready(_)) => {
                            if shown == Some(blob_ref) {
                                continue;
                            }
                            format!("Show full {what}")
                        }
                        Some(BlobFetch::Loading(_)) => format!("Loading full {what}…"),
                        Some(BlobFetch::Failed) => {
                            format!("Couldn't load full {what} — tap to retry")
                        }
                        None => match bytes {
                            Some(b) => format!("Show full {what} ({})", format_kb(b)),
                            None => format!("Show full {what}"),
                        },
                    };
                    return Some(ChipAffordance {
                        blob_ref: blob_ref.clone(),
                        label: SharedString::from(label),
                    });
                }
                None
            })
            .collect();
        // Which chips have their detail block open (render-local, analytic —
        // the FINAL state; a mid-tween detail already counts as its target).
        let detail_folds: Vec<FoldState> = details
            .iter()
            .zip(&invocations)
            .enumerate()
            .map(|(ix, (detail, invocation))| {
                if detail.is_none() && invocation.is_none() {
                    return FoldState::default();
                }
                self.tool_details
                    .get(&SharedString::from(format!("{row_id}#d{ix}")))
                    .copied()
                    .unwrap_or_default()
            })
            .collect();
        let detail_opens: Vec<bool> = details
            .iter()
            .zip(&invocations)
            .zip(&detail_folds)
            .zip(tools.iter())
            .map(|(((detail, invocation), fold), tool)| {
                // A STREAMING thought chip defaults open (the live thinking
                // is the point); settled chips default closed. A user toggle
                // overrides either way.
                let default_open = tool.is_thought && !tool.resolved;
                (detail.is_some() || invocation.is_some()) && fold.open.unwrap_or(default_open)
            })
            .collect();
        let detail_highlights: Vec<Option<Arc<crate::changes::DiffHighlights>>> = details
            .iter()
            .enumerate()
            .map(|(ix, detail)| {
                detail
                    .as_deref()
                    .filter(|_| detail_opens[ix])
                    .and_then(|detail| self.tool_diff_highlight_for(row_id, ix, detail, cx))
            })
            .collect();
        let open_height = chips_height(tools.len())
            + details
                .iter()
                .zip(&invocations)
                .zip(&affordances)
                .zip(&detail_opens)
                .filter(|(_, open)| **open)
                .map(|(((detail, invocation), affordance), _)| {
                    invocation.as_deref().map_or(0.0, detail_height)
                        + detail.as_deref().map_or(0.0, detail_height)
                        + if affordance.is_some() {
                            BLOB_AFFORDANCE_HEIGHT
                        } else {
                            0.0
                        }
                })
                .sum::<f32>();
        let target = if open { open_height } else { 0.0 };
        let summary = tool_group_summary(tools);

        let toggle_id = row_id.clone();
        // A quiet summary sits above the activity rail; its chevron has the
        // same footprint as the tool icons below it.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(ACTIVITY_TEXT_GAP))
            .pr(px(4.0))
            .h(px(26.0))
            .cursor_pointer()
            .text_size(px(TOOL_TEXT_SIZE))
            .line_height(px(18.0))
            // Quiet even when children failed: agents routinely have failed
            // probes mid-work, and a red HEADER read as "this whole step
            // broke" (user report). Failures still show on the individual
            // chips (destructive tint, zeron tool-chip.tsx) and in the
            // summary's "· N failed" count.
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), open_height, auto_open);
                cx.notify();
            }))
            .child(
                div()
                    .w(px(ACTIVITY_GUTTER_WIDTH))
                    .h(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(if open {
                            crate::icons::ALT_ARROW_DOWN
                        } else {
                            crate::icons::ALT_ARROW_RIGHT
                        })
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(tools.iter().enumerate().map(|(ix, tool)| {
                // Spawn chips are LINKS, not accordions: the click opens the
                // subagent's transcript as a right-pane tab (the shell hosts
                // the surface — the chip only announces which doc it indexes).
                if let Some(doc_id) = tool.subagent_ref.clone().filter(|_| is_spawn_link(tool)) {
                    let chat_id = self.chat_id.clone().unwrap_or_default();
                    let title = subagent_tab_title(&tool.call);
                    let frozen = matches!(
                        tool.subagent_status,
                        Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                    );
                    return subagent_chip(
                        tool,
                        SharedString::from(format!("{row_id}#s{ix}")),
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(TranscriptEvent::OpenSubagent {
                                chat_id: chat_id.clone(),
                                doc_id: doc_id.to_string(),
                                title: title.to_string(),
                                frozen,
                            });
                        }),
                        collapses,
                        theme,
                        cx.entity_id(),
                        cx,
                    );
                }
                let detail = details[ix].clone();
                let invocation = invocations[ix].clone();
                if detail.is_none() && invocation.is_none() {
                    return tool_chip(
                        tool,
                        collapses,
                        ix + 1 < tools.len(),
                        theme,
                        cx.entity_id(),
                        cx,
                    );
                }
                let affordance = affordances[ix].clone();
                let affordance_h = if affordance.is_some() {
                    BLOB_AFFORDANCE_HEIGHT
                } else {
                    0.0
                };
                let open = detail_opens[ix];
                let dfold = detail_folds[ix];
                let key = SharedString::from(format!("{row_id}#d{ix}"));
                // Ordinary tools expand into muted text along the same column.
                // Subagent fallbacks retain their card; explicit heights keep
                // the row and group fold animations in sync.
                let closed_h = CHIP_CARD_HEIGHT;
                let open_h = CHIP_CARD_HEIGHT
                    + invocation.as_deref().map_or(0.0, detail_height)
                    + detail.as_deref().map_or(0.0, detail_height)
                    + affordance_h;
                let card_target = if open { open_h } else { closed_h };
                let animating = dfold.epoch > 0
                    && dfold
                        .toggled_at
                        .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
                let toggle_key = key.clone();
                let group_key = row_id.clone();
                let mut card = div()
                    .my(px((CHIP_HEIGHT - CHIP_CARD_HEIGHT) / 2.0))
                    .when(collapses, |el| el.ml(px(ACTIVITY_TEXT_GAP)))
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .when(!collapses, |card| {
                        card.rounded(px(9.0))
                            .border_1()
                            .border_color(crate::theme::hairline(0.07))
                            .bg(crate::theme::ink(0.03))
                    })
                    .child(
                        div()
                            .id(key.clone())
                            .h(px(if collapses {
                                CHIP_CARD_HEIGHT
                            } else {
                                CHIP_HEADER_HEIGHT
                            }))
                            .flex_none()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let entry =
                                    this.tool_details.entry(toggle_key.clone()).or_default();
                                let currently_open = entry.open.unwrap_or(open);
                                entry.from = if currently_open { open_h } else { closed_h };
                                entry.open = Some(!currently_open);
                                entry.epoch += 1;
                                entry.toggled_at = Some(Instant::now());
                                // Arm the GROUP body's height tween too (open
                                // state untouched): the body's height is
                                // analytic over the final detail state, so
                                // without a tween the row snaps to the target
                                // height while the card is still mid-tween —
                                // content below teleported on expand and the
                                // shrinking card clipped on collapse (user
                                // report). `open_height` was computed with
                                // the detail still in its pre-click state,
                                // which is exactly the tween's start; both
                                // tweens share the click instant and the
                                // RESIZE curve, so the row tracks the card's
                                // bottom edge frame-for-frame.
                                let group = this.folds.entry(group_key.clone()).or_default();
                                group.from = open_height;
                                group.epoch += 1;
                                group.toggled_at = Some(Instant::now());
                                cx.notify();
                            }))
                            .child(chip_header(tool, open, theme, cx.entity_id(), cx)),
                    );
                // The body stays mounted while the close tween shrinks over it.
                // Invocation first (what was asked), then output/diff (what
                // came back), separated by a small gap.
                if open || animating {
                    let mut panel = div()
                        .flex_none()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .overflow_hidden();
                    if let Some(invocation) = invocation.as_deref() {
                        panel = panel
                            .child(
                                div()
                                    .h(px(DETAIL_SEPARATOR))
                                    .flex_none()
                                    .when(!collapses, |line| line.bg(crate::theme::hairline(0.06))),
                            )
                            .child(detail_body(invocation, None, theme));
                    }
                    if let Some(detail) = detail.as_deref() {
                        panel = panel
                            .child(
                                div()
                                    .h(px(DETAIL_SEPARATOR))
                                    .flex_none()
                                    .when(!collapses, |line| line.bg(crate::theme::hairline(0.06))),
                            )
                            .child(detail_body(detail, detail_highlights[ix].clone(), theme));
                    }
                    if let Some(ChipAffordance { blob_ref, label }) = affordance {
                        let loading = matches!(
                            self.blob_details.get(&blob_ref),
                            Some(BlobFetch::Loading(_))
                        );
                        let mut row = div()
                            .id(SharedString::from(format!("{key}-blob")))
                            .h(px(BLOB_AFFORDANCE_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .text_size(px(TOOL_TEXT_SIZE))
                            .text_color(theme.text_faint)
                            .child(label);
                        if !loading {
                            row = row
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text_muted))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.spawn_blob_fetch(blob_ref.clone(), cx);
                                    cx.notify();
                                }));
                        }
                        panel = panel.child(row);
                    }
                    card = card.child(panel);
                }
                let card: AnyElement = if animating {
                    let from = dfold.from;
                    card.with_animation(
                        SharedString::from(format!("{key}-tween{}", dfold.epoch)),
                        RESIZE.animation(),
                        move |el, t| el.h(px(motion::lerp(from, card_target, t))),
                    )
                    .into_any_element()
                } else {
                    card.h(px(card_target)).into_any_element()
                };
                let card = div().min_w_0().flex_1().child(card);
                div()
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_row()
                    // Stretch the line alongside the expanded text.
                    .when(collapses, |row| {
                        row.child(activity_rail(tool, ix + 1 < tools.len(), theme))
                    })
                    .child(card)
                    .into_any_element()
            }));

        // Fold body: 200ms committed-height tween on a USER toggle only — and
        // only within a short window of the click. Auto-open (streaming) and
        // content growth never tween, and a SETTLED fold renders at its static
        // height: leaving the tween armed replayed it on every remount, which
        // in a virtualized list means every scroll-back-into-view (only `open`
        // toggles animate — composes with the stick spring). Agent groups skip
        // the fold entirely (always open, no header).
        let animating = collapses
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let body: AnyElement = if !collapses {
            chips.into_any_element()
        } else if animating {
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
            // Tool summaries and cards are code-adjacent chrome. Detail bodies
            // retain their explicit mono/diff typography below this boundary.
            .font_family(theme.font_sans_fixed.clone())
            .when(collapses, |el| el.child(header))
            .child(body)
            .into_any_element()
    }
}

/// A sent message's text with its file-mention chips. The same recipe as the
/// markdown renderer's inline code (`flat_text_element`): chip ranges shape in
/// the mono font at the spectrum's `code_text`, [`StyledText`] supplies wrapped glyph
/// geometry through its layout handle, and a canvas paints the rounded
/// `code_wash` *beneath* the glyphs — so chips wrap, clip, and scroll exactly
/// like the text they decorate.
///
/// Per-frame cost while an assistant message streams below: shaping hits
/// gpui's line-layout cache (identical text + runs ⇒ reuse) and the underlay
/// repaints O(chips) quads — no layout work, no re-projection (spans were
/// computed once in [`rows_for_entry`]).
/// The user bubble's text: runs split at mention-chip boundaries (one plain
/// run when there are none), with the same selection machinery as rendered
/// markdown — the element registers into the frame's document-ordered
/// registry, so drags select, span into adjacent rows, and Cmd+C copies.
fn user_bubble_text(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    theme: &Theme,
    measured_h: Rc<Cell<f32>>,
    entity_id: gpui::EntityId,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, chips read as inline code. Size/line-height flow from the
    // bubble's div like every text child.
    let body_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let chip_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.code_text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::with_capacity(mentions.len() * 2 + 1);
    let mut at = 0;
    for span in mentions.iter() {
        if at < span.range.start {
            runs.push(body_run(span.range.start - at));
        }
        runs.push(chip_run(span.range.len()));
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let wash = theme.code_wash;
    let sel_key: std::sync::Arc<str> = format!("{row_id}:u").into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            for span in mentions.iter() {
                for rect in render::range_rects(&layout, &span.range, 0.0, 2.0) {
                    window.paint_quad(quad(
                        rect,
                        px(5.0),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            render::paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
            // Passive geometry cache only: no entity update and no notify.
            // `bounds().height` can be the collapsed clip height, so derive
            // the full text height from the wrapped line layouts instead. The
            // click handler reads this exact value as the RESIZE endpoint,
            // while idle layout never feeds back into the transcript.
            let line_count: usize = layout
                .line_layouts()
                .iter()
                .map(|line| line.wrap_boundaries.len() + 1)
                .sum();
            let next_h = (line_count.max(1) as f32) * f32::from(layout.line_height());
            if (measured_h.get() - next_h).abs() > 0.5 {
                measured_h.set(next_h);
                // The first layout is the source of truth for soft wrapping.
                // Invalidate the transcript once so the expander and clip are
                // present even when glyph widths make a short-looking string
                // exceed five visual lines.
                cx.notify(entity_id);
            }
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(styled)
        .into_any_element()
}

/// The transcript ErrorChip — a port of zeron chat-view.tsx `ErrorChip`
/// (34px-minimum row, `rounded-[10px] border border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message at `text-foreground/80` — a subtle
/// red-tinted wash, never a bare red-stroke box. Unlike the web port, the
/// message WRAPS instead of truncating: startup-crash errors carry the
/// agent's exit status and stderr, and a one-line ellipsis was exactly what
/// made zeronsh/comet#95 undiagnosable from the screenshot.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .min_h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .py(px(7.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked — the
/// interactive controls live in the composer (chat-view.tsx `InputChip`):
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// A small glyph standing in for the tool's icon (zeron uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (zeron tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::TERMINAL,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        call if is_agent_call(call) => crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// The body of an expanded chip card, under the header's separator. Diffs
/// render through the changes pane's section body — the real component, with
/// hunk headers, dual line-number gutters, accent bars, row washes, and
/// syntax runs — so an inline tool diff is indistinguishable from the
/// checkout diff sidebar. Output renders as a code block: verbatim mono
/// lines, indentation intact, counted-tail truncation.
fn detail_body(
    detail: &ToolDetail,
    diff_highlights: Option<Arc<crate::changes::DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let body = div().w_full().min_w_0().flex().flex_col().overflow_hidden();
    match detail {
        // No comment layer: an inline tool diff is a record of what the
        // agent already did, not a review surface.
        ToolDetail::Diff { file, .. } => body
            .child(crate::changes::render_file_body_with_syntax(
                file,
                diff_highlights,
                theme,
            ))
            .into_any_element(),
        ToolDetail::Stats { stats } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(TOOL_TEXT_SIZE))
            .children(stats.iter().map(|stat| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(stat.path.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.success)
                            .child(SharedString::from(format!("+{}", stat.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("−{}", stat.deletions))),
                    )
            }))
            .into_any_element(),
        ToolDetail::Output {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(TOOL_TEXT_SIZE))
            .children(lines.iter().map(|line| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .text_color(theme.text_faint)
                    .child(div().w_full().min_w_0().truncate().child(line.clone()))
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .text_size(px(TOOL_TEXT_SIZE))
            .children(lines.iter().map(|line| {
                let row = div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center();
                let Some((text, runs)) = thought_line_text(line, theme) else {
                    return row; // blank separator row
                };
                row.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .child(StyledText::new(text).with_runs(runs)),
                )
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
    }
}

/// The counted-tail row under a truncated Output/Thought detail.
fn more_lines_row(truncated_by: usize, theme: &Theme) -> gpui::Div {
    div()
        .h(px(OUTPUT_LINE_HEIGHT))
        .flex()
        .items_center()
        .text_size(px(TOOL_TEXT_SIZE))
        .text_color(theme.text_faint)
        .child(SharedString::from(format!("… {truncated_by} more lines")))
}

/// Shape one flattened thought line into gpui text runs — the detail-body
/// palette: faint foreground prose, semibold for bold, mono for code,
/// underlined links (NOT clickable — a thought is a record, not a surface).
fn thought_line_text(line: &[InlineRun], theme: &Theme) -> Option<(SharedString, Vec<TextRun>)> {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    for run in line {
        if run.text.is_empty() {
            continue;
        }
        let mut f = if run.style.code {
            gpui::font(theme.font_mono.clone())
        } else {
            gpui::font(theme.font_sans_fixed.clone())
        };
        if run.style.bold {
            f.weight = gpui::FontWeight::SEMIBOLD;
        }
        if run.style.italic {
            f.style = gpui::FontStyle::Italic;
        }
        runs.push(TextRun {
            len: run.text.len(),
            font: f,
            color: theme.text_faint,
            background_color: None,
            underline: run.style.link.is_some().then_some(gpui::UnderlineStyle {
                color: Some(theme.text_faint),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_faint),
            }),
        });
        text.push_str(&run.text);
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((text.into(), runs))
}

/// The trailing tile on a chip header, when it has one.
enum ChipTrail {
    /// Expand/collapse chevron — flipped while the detail body is open.
    Chevron { open: bool },
    /// Top-right "opens elsewhere" arrow — the spawn chip's link to its
    /// subagent tab.
    OpenArrow,
}

/// The chip's content row: icon tile + label + detail line (+ trailing tile
/// when the chip expands or links out). Shared between the plain chip, the
/// header of an expandable chip card, and the spawn link chip.
///
/// Spawn chips carry their subagent's lifecycle VISUALLY, in the chip's own
/// language: while running the mini working spinner (the sidebar's) pulses
/// at the right of the ordinary static detail; done is the ordinary quiet
/// chip; failed takes the danger tint — no status words, no live text (a
/// header rewriting itself per stream delta read as noise — user report).
fn chip_header_row(
    tool: &ToolItem,
    trail: Option<ChipTrail>,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content(&tool.call)
    };
    let activity = !is_agent_tool(tool);
    let file_path = match &tool.call {
        ToolCall::ReadFile { path }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. }
        | ToolCall::ApplyPatch { path: Some(path) } => Some(path.as_str()),
        _ => None,
    };
    let running = tool.subagent_ref.is_some()
        && matches!(tool.subagent_status, Some(SubagentStatus::Running));
    let failed = tool.is_error
        || (tool.subagent_ref.is_some()
            && matches!(tool.subagent_status, Some(SubagentStatus::Failed)));
    // Text resolves its color during layout, so group-hover text needs stable
    // child IDs under the keyed, expandable header to retain hover state.
    let hover_text = activity && trail.is_some() && !failed;
    let tint = if failed {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .group("tool-header")
        .h(px(if activity {
            CHIP_CARD_HEIGHT
        } else {
            CHIP_HEADER_HEIGHT
        }))
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(if activity { 0.0 } else { 8.0 }))
        .text_size(px(TOOL_TEXT_SIZE))
        .line_height(px(18.0))
        .when(!activity, |row| {
            row.child(
                // Subagent icon tile (`size-[18px] rounded-[5px] bg-white/[0.08]`,
                // icon size-3).
                div()
                    .size(px(18.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .bg(crate::theme::ink(0.08))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(if tool.is_thought {
                            crate::icons::CHAT_ROUND_LINE
                        } else {
                            tool_icon_path(&tool.call)
                        })
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                    ),
            )
        })
        .child(
            div()
                .flex_none()
                .h(px(18.0))
                .flex()
                .items_center()
                .when(!activity, |label| {
                    label.font_weight(gpui::FontWeight::MEDIUM)
                })
                .text_color(tint)
                .child(SharedString::from(label))
                .map(|label| {
                    if hover_text {
                        label
                            .id("tool-label")
                            .group_hover("tool-header", |style| style.text_color(theme.text))
                            .into_any_element()
                    } else {
                        label.into_any_element()
                    }
                }),
        )
        .child(
            div()
                .when(!activity, |detail| detail.flex_1())
                .min_w_0()
                .h(px(if file_path.is_some() { 24.0 } else { 18.0 }))
                .flex()
                .when(activity && detail.is_empty(), |detail| detail.hidden())
                .items_center()
                .truncate()
                .text_color(if failed {
                    theme.danger
                } else if activity {
                    theme.text_muted
                } else {
                    theme.text.opacity(0.85)
                })
                .child(if let Some(path) = file_path {
                    let badge = div()
                        .min_w_0()
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .px(px(6.0))
                        .rounded(px(5.0))
                        .bg(crate::theme::ink(0.06))
                        .text_color(if failed {
                            theme.danger
                        } else {
                            theme.text.opacity(0.85)
                        })
                        .child(
                            crate::icons::icon(crate::icons::for_file(path))
                                .size(px(14.0))
                                .text_color(tint)
                                .when(activity && !failed, |icon| {
                                    icon.group_hover("tool-header", |style| {
                                        style.text_color(theme.text)
                                    })
                                }),
                        )
                        .child(div().min_w_0().truncate().child(SharedString::from(detail)))
                        .map(|badge| {
                            if hover_text {
                                badge
                                    .id("tool-file-badge")
                                    .group_hover("tool-header", |style| {
                                        style.text_color(theme.text)
                                    })
                                    .into_any_element()
                            } else {
                                badge.into_any_element()
                            }
                        });
                    crate::frost::frosted(5.0, 16.0, badge).into_any_element()
                } else {
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(detail))
                        .into_any_element()
                })
                .map(|detail| {
                    if hover_text {
                        detail
                            .id("tool-detail")
                            .group_hover("tool-header", |style| style.text_color(theme.text))
                            .into_any_element()
                    } else {
                        detail.into_any_element()
                    }
                }),
        )
        .when_some(tool.call.subagent_model(), |row, model| {
            // Which model the child runs on, when the spawn named one.
            //
            // In the trailing slot rather than suffixed onto the detail: the
            // detail is the truncating slot, and the model is exactly what a
            // reader scanning a fan-out of spawns wants left once the
            // descriptions are cut.
            //
            // Bare faint text, NOT a filled pill: the tiles either side of it
            // are AFFORDANCES (the spinner means running, the arrow opens the
            // subagent), so giving a passive label the same chrome made the
            // trailing edge read as three buttons — the loudest thing in the
            // row was the one thing you cannot click.
            row.child(
                div()
                    .flex_none()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(model.to_owned())),
            )
        })
        .when(running, |row| {
            // The sidebar working-row spinner, in the chip's trailing slot —
            // paint-local (fixed footprint), so it never moves the layout.
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                format!(
                    "subagent-chip-{}",
                    tool.subagent_ref.as_deref().unwrap_or_default()
                ),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when_some(trail, |row, trail| {
            // Trailing tile matching the group header's: a chevron for the
            // output/diff accordion, or the open-arrow for spawn chips.
            let tile = div()
                .size(px(18.0))
                .flex_none()
                .when(activity, |tile| {
                    tile.opacity(0.0)
                        .group_hover("tool-header", |style| style.opacity(1.0))
                })
                .when(!activity, |tile| {
                    tile.rounded(px(5.0)).bg(crate::theme::ink(0.06))
                })
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted.opacity(0.8));
            row.child(match trail {
                ChipTrail::Chevron { open } => tile.child(
                    crate::icons::icon(if open {
                        crate::icons::ALT_ARROW_DOWN
                    } else {
                        crate::icons::ALT_ARROW_RIGHT
                    })
                    .size(px(12.0))
                    .text_color(theme.text_faint)
                    .when(activity, |caret| {
                        caret.group_hover("tool-header", |style| {
                            style.text_color(if failed { theme.danger } else { theme.text })
                        })
                    }),
                ),
                ChipTrail::OpenArrow => tile.child(
                    crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.8)),
                ),
            })
        })
}

/// The header row of an expandable chip card.
fn chip_header(
    tool: &ToolItem,
    open: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    chip_header_row(tool, Some(ChipTrail::Chevron { open }), theme, view, cx)
}

/// Max chars a subagent tab title keeps. The strip chip is fixed-width and
/// truncates visually, but the derived title also rides drag ghosts and any
/// future pickers — cap it at the source.
const SUBAGENT_TITLE_MAX: usize = 40;

/// First line of `text`, trimmed, capped at `max` chars with an ellipsis.
fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// Drop a leading "Agent"/"Task" genus (with its `:` and spacing) from a
/// spawn-title candidate. Only a real word boundary strips — "Taskmaster"
/// keeps its name. A bare "Agent"/"Task" strips to "" (no context at all).
fn strip_spawn_prefix(text: &str) -> &str {
    let t = text.trim();
    for prefix in ["agent", "task"] {
        if t.len() >= prefix.len()
            && t.is_char_boundary(prefix.len())
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &t[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    t
}

/// Tab title for a spawn chip's subagent surface: the BARE task description
/// ("verify the marker pipeline"). The chip keeps the tool's fuller name —
/// a fixed-width tab spent on "Agent: " never shows the task, so the genus
/// is stripped here and the call input's description/prompt fields back up
/// a bare name (older docs); "Subagent" only as the last resort.
fn subagent_tab_title(call: &ToolCall) -> SharedString {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Subagent".into(),
    };
    let candidates = [
        Some(name),
        input.and_then(|i| i.get("description")?.as_str()),
        input.and_then(|i| i.get("prompt")?.as_str()),
    ];
    for text in candidates.into_iter().flatten() {
        if let Some(title) = title_line(strip_spawn_prefix(text), SUBAGENT_TITLE_MAX) {
            return title.into();
        }
    }
    "Subagent".into()
}

/// The icon interrupts the rail, leaving a small breathing gap on each side.
/// Absolute line segments stretch with the row, including expanded output.
/// The final row ends at its icon instead of leaving a dangling line.
fn activity_rail(tool: &ToolItem, continues: bool, theme: &Theme) -> gpui::Div {
    let tint = if tool.is_error {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .relative()
        .w(px(ACTIVITY_GUTTER_WIDTH))
        .flex_none()
        .child(
            div()
                .absolute()
                .left(px(12.5))
                .top_0()
                .w(px(1.0))
                .h(px(7.0))
                .bg(crate::theme::hairline(0.12)),
        )
        .when(continues, |rail| {
            rail.child(
                div()
                    .absolute()
                    .left(px(12.5))
                    .top(px(31.0))
                    .bottom_0()
                    .w(px(1.0))
                    .bg(crate::theme::hairline(0.12)),
            )
        })
        .child(
            crate::icons::icon(if tool.is_thought {
                crate::icons::CHAT_ROUND_LINE
            } else {
                tool_icon_path(&tool.call)
            })
            .absolute()
            .left(px(6.0))
            .top(px(12.0))
            .size(px(14.0))
            .text_color(tint),
        )
}

/// A plain activity row, or a card for a subagent without a linked document.
fn tool_chip(
    tool: &ToolItem,
    rail: bool,
    continues: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .when(rail, |row| row.child(activity_rail(tool, continues, theme)))
        .child(
            div()
                .when(rail, |el| el.ml(px(ACTIVITY_TEXT_GAP)))
                .my(px((CHIP_HEIGHT - CHIP_CARD_HEIGHT) / 2.0))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .overflow_hidden()
                .when(!rail, |card| {
                    card.rounded(px(9.0))
                        .border_1()
                        .border_color(crate::theme::hairline(0.07))
                        .bg(crate::theme::ink(0.03))
                })
                .child(chip_header_row(tool, None, theme, view, cx)),
        )
        .into_any_element()
}

/// A spawn chip: same card as [`tool_chip`], but the WHOLE card is the
/// "open the subagent tab" click (open-arrow tile in the trailing slot).
/// No accordion — an inline body would only repeat the subagent's own
/// transcript. The group guide rail is omitted for agent-only rows (no
/// collapse header for it to hang from).
fn subagent_chip(
    tool: &ToolItem,
    id: SharedString,
    on_open: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .id(id)
                .when(rail, |el| el.ml(px(12.0)))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::hairline(0.07))
                .bg(crate::theme::ink(0.03))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.05)))
                .on_click(on_open)
                .child(chip_header_row(
                    tool,
                    Some(ChipTrail::OpenArrow),
                    theme,
                    view,
                    cx,
                )),
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
            is_error,
            resolved,
            subagent_ref,
            subagent_status,
            subagent_tail,
            ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
            // Subagent lifecycle mutates a COMPLETED entry in place (eager-
            // done: the spawn resolves while the subagent runs on) and
            // `byte_len` above doesn't cover these fields — hash them or the
            // cached rows never refresh on status/tail changes.
            acc.push(
                subagent_ref.is_some() as u8
                    | match subagent_status {
                        None => 0,
                        Some(SubagentStatus::Running) => 1 << 1,
                        Some(SubagentStatus::Done) => 2 << 1,
                        Some(SubagentStatus::Failed) => 3 << 1,
                    },
            );
            if let Some(tail) = subagent_tail {
                acc.extend_from_slice(tail.as_bytes());
            }
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
    }
    fnv1a(&acc)
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if record_view_frame("transcript") {
            tracing::warn!(
                distance = self.distance_from_bottom(),
                spring = self.spring_should_run(),
                velocity = self.spring.velocity,
                target_velocity = self.spring.target_vel,
                own_turn = self.own_turn.is_some(),
                veils = self.veils.len(),
                "transcript motion state"
            );
        }
        self.render_cache
            .borrow_mut()
            .retain_rows(&self.rendered_rows);
        self.rendered_rows.clear();
        let code_fences_generation = crate::settings::code_fences_generation(cx);
        if self.code_fences_generation != code_fences_generation {
            self.code_fences_generation = code_fences_generation;
            // Horizontal positions are ephemeral. Reset every block owned by
            // this Transcript even when the toggle originated in another one.
            for runtime in self.code_fences.values() {
                runtime.scroll.set_offset(Point::default());
            }
            // Fit changes every code row from analytic to measured height (or
            // back), including virtual rows outside the current viewport.
            self.list.remeasure();
            if self.pinned {
                self.wake_spring();
            }
            if self.own_turn.is_some() {
                self.own_turn_kick = true;
            }
        }
        let typography_generation = crate::typography::generation(cx);
        if self.typography_generation != typography_generation {
            self.typography_generation = typography_generation;
            // `refresh_windows` re-lays out visible rows, but ListState keeps
            // measured heights for virtualized rows outside the viewport.
            // Mark every row unmeasured while retaining height hints and a
            // proportional scroll anchor; GPUI will refresh each measurement
            // as the row enters its layout range.
            self.list.remeasure();
        }
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::attachments::flush_evicted(Some(window), cx);
        // Own-turn driver: measurements are only authoritative after layout,
        // so reservation sizing, the send glide, and the outgrown-handoff
        // each advance at most once per requested frame. Scheduled on every
        // frame while an anchor is live (not just on kicks) so viewport
        // resizes and streaming growth re-derive the reservation; the step
        // only notifies on change, so a settled hold schedules no next frame.
        if (self.own_turn.is_some() || self.own_turn_kick) && !self.own_turn_scheduled {
            self.own_turn_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.own_turn_scheduled = false;
                        this.step_own_turn(cx);
                    })
                    .ok();
            });
        }
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        // Programmatic `scroll_to` does not invoke the list's user-scroll
        // handler. Refresh distance-derived state once layout has measured the
        // replay, guarded so a stale A callback cannot mutate B (or a newer A).
        if self.viewport_finalize_pending && !self.viewport_finalize_scheduled {
            self.viewport_finalize_scheduled = true;
            let token = ViewportFinalizeToken {
                generation: self.viewport_generation,
                layout_revision: self.viewport_layout_revision,
            };
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.viewport_finalize_scheduled = false;
                        if !token.still_current(this.viewport_generation) {
                            if this.viewport_finalize_pending {
                                cx.notify();
                            }
                            return;
                        }
                        let distance = this.distance_from_bottom();
                        this.last_scroll_distance = distance;
                        this.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX
                            && !this.pinned
                            && !this.own_turn.as_ref().is_some_and(|turn| turn.held);
                        if token.layout_settled(this.viewport_layout_revision) {
                            this.viewport_finalize_pending = false;
                        }
                        cx.notify();
                    })
                    .ok();
            });
        }
        // A long-message collapse near the bottom owns the viewport for the
        // duration of its height tween. Advance the matching upward scroll once
        // per frame so the bubble stays visible instead of shrinking above the
        // fixed viewport while the bottom content remains on screen.
        if self.user_collapse_scroll.is_some() && !self.user_collapse_scroll_scheduled {
            self.user_collapse_scroll_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.user_collapse_scroll_scheduled = false;
                        this.step_user_collapse_scroll(cx);
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        self.update_runway_minimum();
        let list_el = list(self.list.clone(), cx.processor(Self::render_row))
            .size_full()
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let content: AnyElement = if self.doc_override.is_some() {
            // The primary transcript's fade lives on the SHELL's outlet
            // wrapper (it spans the titlebar/composer chrome); an override
            // instance owns its own — top edge only (nothing overlays the
            // pane's bottom), gated on real overflow so a short top-anchored
            // transcript shows no fade. Gated here rather than at paint via
            // a ScrollHandle (the list isn't one); scrolls re-render this
            // entity, so the flag can't go stale.
            let scrolled_under_top = {
                let max = f32::from(self.list.max_offset_for_scrollbar().y);
                max - self.distance_from_bottom() > 1.0
            };
            crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                scrolled_under_top,
                false,
                list_el,
            )
            .into_any_element()
        } else {
            list_el.into_any_element()
        };
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::on_selection_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            .child(content)
            .child(rail);
        // Full-size viewer for a clicked user-bubble thumbnail
        // (AttachmentPreviewDialog: bare lightbox, click closes).
        if let Some(preview) = self.attachment_preview.clone() {
            let weak = cx.weak_entity();
            return root.child(crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                &self.attachment_preview_focus,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.attachment_preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::MessagePart;

    #[test]
    fn selection_scroll_ramps_at_viewport_edges() {
        let bounds = Bounds::new(
            gpui::point(px(10.0), px(20.0)),
            gpui::size(px(300.0), px(200.0)),
        );
        assert_eq!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(120.0))),
            0.0
        );
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(20.0))) < 0.0);
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0))) > 0.0);
        assert!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0)))
                > selection_scroll_step(bounds, gpui::point(px(20.0), px(200.0)))
        );
    }

    // ---- streaming parse wiring (the transcript side, not the parser) ----

    #[test]
    fn live_row_parse_work_is_bounded_per_commit() {
        // Drive the EXACT wiring `rows_for` uses (`parse_for_row`) with the
        // prefix-extending commit snapshots the doc watch delivers, and prove
        // the per-commit parse work stays O(reparsed tail): a full-reparse
        // wiring would feed ~N/2 × final_len bytes through the parser across N
        // commits; the incremental path stays within a small multiple of the
        // final length regardless of N.
        let mut live_parsers = HashMap::new();
        let mut tree_cache = HashMap::new();
        let paragraph = "A paragraph of streaming prose that keeps arriving.\n\n";
        let commits = 120usize;
        let mut text = String::new();
        let mut total_parsed = 0usize;
        for i in 0..commits {
            // Each commit appends ~half a paragraph (crosses block boundaries).
            let chunk = &paragraph[..paragraph.len() / 2];
            text.push_str(if i % 2 == 0 {
                chunk
            } else {
                &paragraph[paragraph.len() / 2..]
            });
            let (tree, outcome) =
                parse_for_row(true, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
            assert!(!tree.blocks.is_empty());
            let ParseOutcome::Incremental {
                parsed_bytes,
                stable_prefix_blocks,
            } = outcome
            else {
                panic!("streaming commit must take the incremental path");
            };
            total_parsed += parsed_bytes;
            // Per commit: never a full reparse once the doc has grown past the
            // tail window (last two complete blocks + the partial trailing
            // one + the delta ≤ 3 paragraphs here).
            assert!(
                parsed_bytes <= 3 * paragraph.len(),
                "commit {i}: parsed {parsed_bytes} bytes — not bounded by the tail window"
            );
            // The stable prefix grows with the doc — settled blocks are never
            // re-touched (this is what keeps render caches valid).
            assert!(stable_prefix_blocks + 2 >= tree.blocks.len().saturating_sub(1));
        }
        // Across the whole stream: work is commits × O(tail), an order of
        // magnitude under the ~commits × len/2 a full-reparse wiring costs.
        let final_len = text.len();
        let full_reparse_cost = commits * final_len / 2;
        assert!(total_parsed <= commits * 3 * paragraph.len());
        assert!(
            total_parsed * 10 < full_reparse_cost,
            "total parsed {total_parsed} vs full-reparse ~{full_reparse_cost}"
        );

        // Live→complete handoff: the completed part adopts the live parser's
        // exact tree without parsing a single byte.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Handoff);
        // And the settled cache serves repeats with no work at all.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Cached);
    }

    // ---- stick-to-bottom spring ----

    #[test]
    fn stationary_spring_does_not_keep_requesting_frames() {
        let mut spring = StickSpring::new();
        let mut pos = 600.0;
        for _ in 0..120 {
            let next = spring.step(pos, 600.0, 1.0);
            assert_eq!(next, pos);
            assert!(!StickSpring::needs_frame(600.0 - next));
            pos = next;
        }
        // Real growth must still wake and complete the same smooth glide.
        let target = 900.0;
        let mut moving_frames = 0;
        while StickSpring::needs_frame(target - pos) && moving_frames < 600 {
            let next = spring.step(pos, target, 1.0);
            assert!(next >= pos && next <= target);
            pos = next;
            moving_frames += 1;
        }
        assert_eq!(pos, target);
        assert!(moving_frames > 1 && moving_frames < 600);
        assert!(!StickSpring::needs_frame(0.0));
    }

    #[test]
    fn estimated_height_growth_at_the_bottom_cannot_keep_spring_awake() {
        let mut spring = StickSpring::new();
        // Virtualized height estimates can grow while the viewport remains
        // anchored to exactly the same final row. This previously kept the
        // feed-forward velocity and the redraw loop alive after completion.
        for frame in 0..120 {
            let target = 10000.0 + frame as f32 * 400.0;
            let next = spring.step(target, target, 1.0);
            assert_eq!(next, target);
            assert!(!StickSpring::needs_frame(target - next));
        }
        assert!(spring.target_vel() > 1.0, "exercise a nonzero estimate");
    }

    #[test]
    fn spring_converges_to_a_fixed_target() {
        let mut spring = StickSpring::new();
        let target = 400.0;
        let mut pos = 0.0;
        let mut frames = 0;
        while pos < target && frames < 600 {
            pos = spring.step(pos, target, 1.0);
            frames += 1;
        }
        assert_eq!(pos, target, "spring must land exactly on the target");
        assert!(
            frames < 300,
            "400px should converge within 5s of frames, took {frames}"
        );
        // Once landed it stays landed (and idles out).
        for _ in 0..120 {
            pos = spring.step(pos, target, 1.0);
            assert_eq!(pos, target);
        }
        assert!(spring.is_idle(), "no residual motion at rest");
    }

    #[test]
    fn spring_never_overshoots_or_oscillates() {
        let mut spring = StickSpring::new();
        let target = 250.0;
        let mut pos = 0.0;
        let mut last = pos;
        for _ in 0..600 {
            pos = spring.step(pos, target, 1.0);
            assert!(pos <= target, "overshoot: {pos} > {target}");
            assert!(
                pos >= last - 1e-3,
                "oscillation: position moved backwards {last} -> {pos}"
            );
            last = pos;
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn spring_feed_forward_tracks_constant_growth() {
        // Target grows 2px/frame (≈120px/s — a typical stream). After warmup
        // the EMA feed-forward must carry the viewport at the same rate with a
        // bounded, stable lag — a glide, not 0,0,0,Npx steps.
        let growth = 2.0;
        let mut spring = StickSpring::new();
        let mut target = 600.0;
        let mut pos = 600.0;
        let mut deltas: Vec<f32> = Vec::new();
        for frame in 0..400 {
            target += growth;
            let next = spring.step(pos, target, 1.0);
            if frame >= 200 {
                deltas.push(next - pos);
            }
            pos = next;
        }
        // Steady state: per-frame movement ≈ growth rate…
        let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
        assert!(
            (mean - growth).abs() < 0.2,
            "steady-state speed {mean} should track growth {growth}"
        );
        // …with no stepping (every frame moves, none jumps).
        for d in &deltas {
            assert!(*d > 0.0, "viewport stalled mid-stream");
            assert!(*d < growth * 3.0, "viewport jumped: {d}px in one frame");
        }
        // The EMA growth estimate itself has locked on.
        assert!((spring.target_vel() - growth).abs() < 0.3);
        // Lag stays bounded by the chase lead.
        assert!(target - pos <= SPRING_CHASE_MAX_LEAD + growth);
    }

    #[test]
    fn spring_feed_forward_resets_when_target_shrinks() {
        let mut spring = StickSpring::new();
        let mut pos = 0.0;
        for i in 1..=50 {
            pos = spring.step(pos, 100.0 + i as f32 * 4.0, 1.0);
        }
        assert!(spring.target_vel() > 1.0);
        // A collapse (target shrinks by more than 1px) drops the estimate.
        spring.step(pos.min(120.0), 120.0, 1.0);
        assert_eq!(spring.target_vel(), 0.0);
    }

    #[test]
    fn spring_catchup_frames_glide_instead_of_teleporting() {
        // A 5-frame hitch advances roughly as far as 5 single steps would —
        // sub-stepped, still clamped at the target.
        let target = 300.0;
        let mut a = StickSpring::new();
        let mut pos_a = 0.0;
        for _ in 0..5 {
            pos_a = a.step(pos_a, target, 1.0);
        }
        let mut b = StickSpring::new();
        let pos_b = b.step(0.0, target, 5.0);
        assert!((pos_a - pos_b).abs() < 1.0, "{pos_a} vs {pos_b}");
        assert!(pos_b <= target);
    }

    #[test]
    fn restick_is_direction_aware() {
        // Scrolling away from the bottom never resticks, even inside the band
        // (a 20px wheel notch from the pinned bottom must break the pin).
        assert!(!Transcript::should_restick(20.0, 0.0));
        assert!(!Transcript::should_restick(69.0, 30.0));
        // Returning toward the bottom resticks once inside the 70px band…
        assert!(Transcript::should_restick(69.0, 120.0));
        assert!(Transcript::should_restick(0.0, 30.0));
        // …but not while still outside it.
        assert!(!Transcript::should_restick(200.0, 300.0));
        // No movement — leave the pin alone.
        assert!(!Transcript::should_restick(50.0, 50.0));
    }

    #[test]
    fn only_a_stream_at_the_bottom_gets_a_hard_end_anchor() {
        assert!(should_anchor_live_stream(true, 0.0, true));
        assert!(should_anchor_live_stream(true, AT_BOTTOM_PX, true));

        // A user who has moved away from the end keeps control of the
        // viewport, even if the transcript is still streaming.
        assert!(!should_anchor_live_stream(true, AT_BOTTOM_PX + 0.1, true));
        assert!(!should_anchor_live_stream(false, 0.0, true));

        // Ordinary transcript updates retain the existing spring behavior.
        assert!(!should_anchor_live_stream(true, 0.0, false));
    }

    fn viewport_row(id: &str, entry_id: &str) -> Row {
        Row {
            id: id.into(),
            version: 0,
            turn_start: true,
            kind: RowKind::ErrorChip {
                message: SharedString::default(),
            },
            entry_id: entry_id.into(),
            timestamp: None,
            copy_text: None,
        }
    }

    #[test]
    fn viewport_anchor_tracks_a_stable_row_across_replay() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 1,
                offset_in_item: px(23.0),
            },
        )
        .expect("visible row");

        let replay = vec![
            viewport_row("new", "entry-new"),
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let restored = anchor.resolve(&replay).expect("restored row");
        assert_eq!(restored.item_ix, 2);
        assert_eq!(restored.offset_in_item, px(23.0));
    }

    #[test]
    fn viewport_anchor_has_entry_and_index_fallbacks() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("old-block", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 2,
                offset_in_item: px(31.0),
            },
        )
        .expect("visible row");

        let reshaped = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("inserted", "entry-new"),
            viewport_row("new-block", "entry-c"),
        ];
        let same_entry = anchor.resolve(&reshaped).expect("entry fallback");
        assert_eq!(same_entry.item_ix, 3);
        assert_eq!(same_entry.offset_in_item, px(0.0));

        let entry_removed = vec![viewport_row("a", "entry-a"), viewport_row("b", "entry-b")];
        let clamped = anchor.resolve(&entry_removed).expect("index fallback");
        assert_eq!(clamped.item_ix, 1);
        assert_eq!(clamped.offset_in_item, px(0.0));
    }

    #[test]
    fn optimistic_echo_cannot_consume_a_historical_viewport_before_replay() {
        let history = vec![viewport_row("historical", "historical-entry")];
        let saved = SavedViewport::capture(&history, ListOffset::default(), false, 480.0, None)
            .expect("historical viewport");
        let echo_only = vec![viewport_row("echo", "echo-entry")];

        assert!(
            saved.resolve(&echo_only, false).is_none(),
            "an unrelated echo is not an authoritative index fallback"
        );
        assert_eq!(
            saved
                .resolve(&echo_only, true)
                .expect("populated replay may use an index fallback")
                .offset
                .item_ix,
            0
        );
        assert!(TranscriptReplayState::Empty.authoritative_empty());
        assert!(!TranscriptReplayState::Empty.allows_fallback());
        assert!(!TranscriptReplayState::Pending.allows_fallback());
        assert!(TranscriptReplayState::Populated.allows_fallback());

        let echo_viewport =
            SavedViewport::capture(&echo_only, ListOffset::default(), false, 0.0, None)
                .expect("echo viewport");
        assert!(
            echo_viewport.resolve(&echo_only, false).is_some(),
            "the exact optimistic row is safe before replay"
        );
    }

    #[test]
    fn saved_viewport_preserves_and_releases_an_active_turn_runway() {
        let rows = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &rows,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
            false,
            0.0,
            Some(&own_turn),
        )
        .expect("active chat viewport");
        let SavedViewport::Anchored {
            own_turn: Some(saved_turn),
            ..
        } = &saved
        else {
            panic!("an active turn must keep its runway with the viewport");
        };
        assert!(saved_turn.held);
        assert!(saved_turn.positioned);

        let restored = saved
            .resolve(&rows, false)
            .expect("exact queued echo survives an empty replay");
        let restored_turn = restored.own_turn.expect("valid restored runway");
        assert!(!restored_turn.held);
        assert!(!restored_turn.positioned);
        assert!(restored_turn.seen_prompt);

        let list_state = ListState::new(rows.len(), ListAlignment::Bottom, px(0.0));
        list_state.reset(0);
        list_state.splice(0..0, rows.len());
        list_state.scroll_to(restored.offset);
        assert_eq!(list_state.logical_scroll_top().item_ix, 0);
        assert_eq!(list_state.logical_scroll_top().offset_in_item, px(0.0));

        assert!(
            SavedViewport::capture(&[], ListOffset::default(), false, 0.0, Some(&own_turn))
                .is_none(),
            "an empty rapid-switch replay must not overwrite the older snapshot"
        );
    }

    #[test]
    fn own_turn_waits_for_its_first_echo_then_retires_if_it_disappears() {
        let mut turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            held: true,
            positioned: false,
            seen_prompt: false,
        };

        assert!(turn.observe_prompt(false), "fresh send waits one state gap");
        assert!(turn.observe_prompt(true), "echo activates the runway");
        assert!(turn.seen_prompt);
        assert!(
            !turn.observe_prompt(false),
            "failed echo retires the activated runway"
        );
    }

    #[test]
    fn restored_viewport_discards_a_failed_optimistic_turn() {
        let outgoing = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &outgoing,
            ListOffset::default(),
            false,
            420.0,
            Some(&own_turn),
        )
        .expect("outgoing viewport");

        // The failed echo vanished while A was hidden. The ordinary viewport
        // still restores by index, but no stale runway may intercept jump.
        let replay = vec![viewport_row("older", "older")];
        let restored = saved.resolve(&replay, true).expect("index fallback");
        assert!(restored.own_turn.is_none());
        assert_eq!(restored.offset.item_ix, 0);
        assert_eq!(restored.distance_from_bottom, 420.0);
    }

    #[test]
    fn pinned_viewports_follow_tail_and_the_cache_is_bounded() {
        let rows = vec![viewport_row("row", "entry")];
        let pinned = SavedViewport::capture(&rows, ListOffset::default(), true, 999.0, None)
            .expect("pinned viewport");
        assert!(matches!(pinned, SavedViewport::FollowTail));

        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS + 8 {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }
        assert_eq!(cache.len(), MAX_SAVED_VIEWPORTS);
        assert!(cache.get_cloned_and_touch("chat-0").is_none());
        assert!(
            cache
                .get_cloned_and_touch(&format!("chat-{}", MAX_SAVED_VIEWPORTS + 7))
                .is_some()
        );
    }

    #[test]
    fn reopening_the_oldest_cached_chat_protects_it_from_the_next_eviction() {
        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }

        assert!(cache.get_cloned_and_touch("chat-0").is_some());
        cache.insert("outgoing-new".into(), SavedViewport::FollowTail);

        assert!(cache.by_chat.contains_key("chat-0"));
        assert!(!cache.by_chat.contains_key("chat-1"));
        assert!(cache.by_chat.contains_key("outgoing-new"));
    }

    #[test]
    fn viewport_finalization_waits_for_current_generation_and_stable_layout() {
        let token = ViewportFinalizeToken {
            generation: 7,
            layout_revision: 11,
        };
        assert!(token.still_current(7));
        assert!(!token.still_current(8));
        assert!(token.layout_settled(11));
        assert!(!token.layout_settled(12));
    }

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

    fn reasoning_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Reasoning {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn reasoning_joins_the_tool_group_accordion() {
        // Thought → tool → thought → tool folds into ONE group row (user
        // request: the thought process lives inside the combined accordion),
        // and the collapsed summary names the thinking.
        let entry = assistant(
            "a1",
            MessageStatus::Complete,
            vec![
                reasoning_part("r0", "planning the first step"),
                tool_part("t1", "ls"),
                reasoning_part("r2", "now the second step"),
                tool_part("t3", "pwd"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one combined accordion row");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(tools.len(), 4);
        assert!(tools[0].is_thought && tools[2].is_thought);
        assert!(!tools[1].is_thought && !tools[3].is_thought);
        // Thought chips carry their text as a styled-line detail with an
        // ANALYTIC height, so the group's fold tween covers them.
        assert!(matches!(
            tools[0].detail.as_deref(),
            Some(ToolDetail::Thought { lines, .. }) if !lines.is_empty()
        ));
        let summary = tool_group_summary(&tools);
        assert!(summary.starts_with("Thought 2 times"), "{summary}");
        assert!(summary.contains("2 commands"), "{summary}");

        // A lone thought is still an accordion (with the group tween), named
        // plainly.
        let entry = assistant(
            "a2",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "just thinking")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(tool_group_summary(&tools), "Thought process");

        // Empty reasoning renders nothing.
        let entry = assistant(
            "a3",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }

    #[test]
    fn live_thought_streams_open_and_settles_closed() {
        let entry = assistant(
            "a1",
            MessageStatus::Streaming,
            vec![reasoning_part("r0", "thinking hard")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        // The live tail auto-opens the group; the chip itself is unresolved
        // (defaults open) until the part stops being the tail.
        assert!(*auto_open);
        assert!(!tools[0].resolved);

        let entry = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![
                reasoning_part("r0", "thinking hard"),
                text_part("t1", "answer"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert!(tools[0].resolved, "a followed thought is settled");
    }

    fn thought_of(text: &str) -> Vec<Vec<InlineRun>> {
        thought_lines(&parse_full(text))
    }

    fn line_chars(line: &[InlineRun]) -> usize {
        line.iter().map(|r| r.text.chars().count()).sum()
    }

    fn line_string(line: &[InlineRun]) -> String {
        line.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn codex_summary_paragraphs_render_as_separate_styled_lines() {
        let lines = thought_of("**Implementing file badges**\n\n**Preparing fixture screenshots**");
        assert_eq!(
            lines
                .iter()
                .map(|line| line_string(line))
                .collect::<Vec<_>>(),
            [
                "Implementing file badges",
                "",
                "Preparing fixture screenshots"
            ]
        );
        for ix in [0, 2] {
            assert!(
                lines[ix]
                    .iter()
                    .filter(|run| !run.text.is_empty())
                    .all(|run| run.style.bold)
            );
        }
    }

    #[test]
    fn thought_wrap_is_word_aware_and_bounded() {
        let lines = thought_of("one two three");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_string(&lines[0]), "one two three");
        let long = "word ".repeat(200);
        let lines = thought_of(&long);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        assert!(lines.len() > 5);
        let pathological = "x".repeat(300);
        let lines = thought_of(&pathological);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        // A word glued across style boundaries wraps as ONE unit — no line
        // may split inside `**bold**tail`.
        let glued = format!("{} **bold**tail", "word ".repeat(30));
        let lines = thought_of(&glued);
        let joined: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        assert!(joined.iter().any(|l| l.ends_with("boldtail")), "{joined:?}");
    }

    #[test]
    fn thought_markdown_styles_instead_of_literal_markers() {
        // The exact user report: `**bold**` markers showed as glyphs.
        let lines = thought_of("**Planning rollback** then *checking* `parse` [docs](https://d)");
        assert_eq!(lines.len(), 1);
        let flat = line_string(&lines[0]);
        assert!(
            !flat.contains('*') && !flat.contains('`') && !flat.contains('['),
            "{flat}"
        );
        let line = &lines[0];
        assert!(
            line.iter()
                .any(|r| r.style.bold && r.text.contains("Planning rollback")),
            "bold run survives: {line:?}"
        );
        assert!(
            line.iter()
                .any(|r| r.style.italic && r.text.contains("checking"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.code && r.text.contains("parse"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.link.is_some() && r.text.contains("docs"))
        );
    }

    #[test]
    fn thought_blocks_flatten_structurally() {
        let lines = thought_of("# Head\n\npara\n\n- one\n- two\n\n```rust\nlet x = 1;\n```");
        let flat: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        // Heading renders bold, same size (one 18px row).
        assert!(
            lines[0]
                .iter()
                .any(|r| r.style.bold && r.text.contains("Head"))
        );
        // Blank separator rows between top-level blocks; tight list inside.
        assert_eq!(flat[1], "");
        assert_eq!(flat[2], "para");
        assert_eq!(flat[4], "• one");
        assert_eq!(flat[5], "• two");
        // Code lines verbatim, styled as code (mono at render).
        assert!(
            lines
                .last()
                .unwrap()
                .iter()
                .any(|r| r.style.code && r.text == "let x = 1;"),
            "{flat:?}"
        );
    }

    fn tool_part(id: &str, command: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

    #[test]
    fn live_entry_splits_per_block_with_id_continuity() {
        // Live rows split per block exactly like completed ones (the list
        // virtualizes them — the fading tail is the only per-frame work).
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        assert_eq!(live_rows.len(), 3, "one live row per top-level block");
        assert!(
            live_rows
                .iter()
                .all(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
        );
        assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");
        assert_eq!(live_rows[2].id.as_ref(), "m1#t0.2");

        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        assert_eq!(done_rows.len(), 3, "three top-level blocks");
        // Every block row keeps its id across the flip — no flicker on handoff.
        for (live, done) in live_rows.iter().zip(&done_rows) {
            assert_eq!(live.id, done.id);
            // The flip changes the version even at identical text (the
            // streaming bit), forcing a splice.
            assert_ne!(live.version, done.version);
        }
        assert!(matches!(
            done_rows[0].kind,
            RowKind::Markdown { block_ix: 0, .. }
        ));
    }

    #[test]
    fn live_commit_changes_only_tail_row_versions() {
        // Streaming commit: appending to the last block leaves every settled
        // block row's (id, version) untouched — the diff splices only the tail.
        let t1 = "para one\n\npara two\n\npara three";
        let t2 = "para one\n\npara two\n\npara three grows here";
        let live1 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t1)]);
        let live2 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t2)]);
        let r1 = rows_for_entry(&live1, false, &mut parse);
        let r2 = rows_for_entry(&live2, false, &mut parse);
        assert_eq!(r1.len(), 3);
        assert_eq!(r2.len(), 3);
        assert_eq!(r1[0].version, r2[0].version, "settled block untouched");
        assert_eq!(r1[1].version, r2[1].version, "settled block untouched");
        assert_ne!(r1[2].version, r2[2].version, "tail block respliced");
        assert_eq!(diff_rows(&r1, &r2), Some((2..3, 1)));
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
        assert_eq!(top_gap_for(Some(&rows[0]), &rows[1]), render::MD_BLOCK_GAP);
        assert_eq!(top_gap_for(Some(&rows[1]), &rows[2]), render::MD_BLOCK_GAP);
        // Markdown → tool group and tool group → next part: larger boundary.
        assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), Theme::SPACE_MD);
        assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), Theme::SPACE_MD);
        // Turn starts get the turn gap regardless.
        assert_eq!(top_gap_for(None, &rows[0]), Theme::SPACE_LG);
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

    fn agent_part(id: &str, description: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: format!("Agent: {description}"),
                input: Some(serde_json::json!({ "description": description })),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: Some(format!("chat--sub--{id}")),
            subagent_status: Some(SubagentStatus::Running),
            subagent_tail: None,
        }
    }

    #[test]
    fn agent_calls_split_out_of_ordinary_tool_groups() {
        // Agent/spawn chips must not share a collapse with Reads/Runs: a
        // lone Agent used to hide behind "Called 1 tool", and a mixed
        // group hid the running subagent until the user opened the fold.
        let entry = assistant(
            "m-agent",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                agent_part("s1", "Map URL import ingest path"),
                tool_part("c", "make"),
                agent_part("s2", "Audit the fold path"),
                agent_part("s3", "Verify the commit cadence"),
                text_part("t1", "after"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(
            ids,
            [
                "m-agent#t0.0",
                "m-agent#g0",
                "m-agent#g1",
                "m-agent#g2",
                "m-agent#g3",
                "m-agent#t1.0",
            ]
        );

        let RowKind::ToolGroup { tools, auto_open } = &rows[1].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(tool_group_collapses(tools));
        assert!(!*auto_open);

        let RowKind::ToolGroup { tools, .. } = &rows[2].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_tool(&tools[0]));

        let RowKind::ToolGroup { tools, .. } = &rows[3].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(tool_group_collapses(tools));

        let RowKind::ToolGroup { tools, .. } = &rows[4].kind else {
            panic!("consecutive agents share a group")
        };
        assert_eq!(tools.len(), 2);
        assert!(!tool_group_collapses(tools));
        assert!(tools.iter().all(is_agent_tool));
    }

    #[test]
    fn stray_subagent_ref_on_a_run_chip_stays_an_ordinary_tool() {
        // Docs written before the claude-driver fix carry subagent refs on
        // ordinary Run chips (a background shell's task_notification was
        // mis-tagged as subagent traffic). The ref alone must not change the
        // chip's genus: it folds with its neighbors and renders as a plain
        // tool, never as a spawn link to a doc that was never created.
        let mut stray = tool_part("b", "git clone …");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut stray
        {
            *subagent_ref = Some("chat--sub--b".into());
            *subagent_status = Some(SubagentStatus::Done);
        }
        let entry = assistant(
            "m-stray",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), stray, tool_part("c", "make")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one folded group, no agent split");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("tool group expected")
        };
        assert_eq!(tools.len(), 3);
        assert!(tool_group_collapses(tools));
        assert!(tools.iter().all(|t| !is_agent_tool(t)));
        assert!(tools.iter().all(|t| !is_spawn_link(t)));
    }

    #[test]
    fn lone_completed_agent_stays_uncollapsed() {
        let entry = assistant(
            "m-lone",
            MessageStatus::Complete,
            vec![agent_part("s1", "scan repo")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools), "no 'Called 1 tool' wrap");
        assert!(
            !*auto_open,
            "auto_open is a streaming flag; agent rows ignore it at paint"
        );
    }

    #[test]
    fn pre_spawn_agent_name_is_enough_to_split() {
        // Before the engine stamps subagent_ref the chip is already named
        // "Agent: …" — that genus must split, or the spawn hides until the
        // first tagged event.
        let mut part = agent_part("s1", "scan repo");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut part
        {
            *subagent_ref = None;
            *subagent_status = None;
        }
        let entry = assistant(
            "m-pre",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), part],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 2);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!()
        };
        assert!(tool_group_collapses(tools));
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!()
        };
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_call(&tools[0].call));
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

    // Exercise the real Transcript handlers with GPUI's Linux headless
    // platform; no renderer, display server, or test-only dependency needed.
    #[cfg(target_os = "linux")]
    mod user_fold_scroll {
        use super::*;

        fn with_transcript(test: impl FnOnce(&mut Transcript, &mut Context<Transcript>) + 'static) {
            gpui_platform::headless().run(move |cx| {
                let state = cx.new(|_| AppState::new());
                let transcript = cx.new(|cx| Transcript::new(state, cx));
                transcript.update(cx, test);
                // Quit after the platform loop starts; calloop resets its
                // stop flag on entry, so quitting in the launch hook hangs.
                cx.spawn(async move |cx| {
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
        }

        fn with_window(
            test: impl FnOnce(Entity<Transcript>, gpui::WindowHandle<Transcript>, &mut gpui::App)
            + 'static,
        ) {
            gpui_platform::headless().run(move |cx| {
                cx.set_global(Theme::dark());
                let state = cx.new(|_| AppState::new());
                let window = cx
                    .open_window(
                        gpui::WindowOptions {
                            window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                                Point::default(),
                                gpui::size(px(1000.0), px(800.0)),
                            ))),
                            ..Default::default()
                        },
                        |_, cx| cx.new(|cx| Transcript::new(state, cx)),
                    )
                    .unwrap();
                test(window.entity(cx).unwrap(), window, cx);
                cx.spawn(async move |cx| {
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
        }

        fn draw(window: gpui::WindowHandle<Transcript>, cx: &mut gpui::App) {
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            })
            .unwrap();
        }

        // These exercise frame-by-frame geometry, including the first paint
        // after a row append. Eventual settling alone misses visible jumps.
        #[test]
        fn runway_short_chat_glides_from_its_bottom_aligned_position() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = vec![viewport_row("prompt", "prompt")];
                    this.list.reset(1);
                    this.rail_enabled = false;
                    cx.notify();
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                let mut previous = transcript.read(cx).list.bounds_for_item(0).unwrap().top();
                assert!(previous > px(400.0));
                for _ in 0..80 {
                    transcript.update(cx, |this, cx| {
                        this.own_turn_last_tick = Some(Instant::now() - Duration::from_millis(17));
                        this.step_own_turn(cx);
                    });
                    draw(window, cx);
                    let top = transcript.read(cx).list.bounds_for_item(0).unwrap().top();
                    assert!(top <= previous + px(0.5), "glide reversed");
                    assert!(
                        previous - top < px(150.0),
                        "glide jumped: {previous:?} -> {top:?}"
                    );
                    previous = top;
                }
                assert!(previous.abs() < px(1.0));
            });
        }

        fn append_runway_rows(this: &mut Transcript, count: usize, cx: &mut Context<Transcript>) {
            let old_last = this.rows.len() - 1;
            let mut rows = this.rows.clone();
            for ix in 0..count {
                rows.push(viewport_row(&format!("reply-{}", old_last + ix), "reply"));
            }
            // Isolate the row-splice layout boundary; the streaming tests
            // below exercise the real row builder and sync as well.
            this.list.splice(this.rows.len()..this.rows.len(), count);
            this.rows = rows;
            this.list.remeasure_items(old_last..old_last + 1);
            this.remeasure_last_row();
            this.own_turn_kick = true;
            cx.notify();
        }

        #[test]
        fn runway_append_consumes_space_before_the_first_paint() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = vec![viewport_row("prompt", "prompt")];
                    this.list.reset(1);
                    this.rail_enabled = false;
                    this.on_own_send("chat".into(), "prompt".into(), cx);
                    this.list.scroll_to(ListOffset::default());
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| this.step_own_turn(cx));
                draw(window, cx);
                let before = transcript.read(cx).list.max_offset_for_scrollbar().y;
                transcript.update(cx, |this, cx| append_runway_rows(this, 3, cx));
                draw(window, cx);
                let after = transcript.read(cx).list.max_offset_for_scrollbar().y;
                assert!(
                    (after - before).abs() <= px(1.0),
                    "append exposed blank scroll space: {before:?} -> {after:?}"
                );
            });
        }

        fn feed(
            this: &mut Transcript,
            entries: Vec<SessionMessageEntry>,
            cx: &mut Context<Transcript>,
        ) {
            this.state.update(cx, |state, _| {
                state.selected_chat = Some("chat".into());
                state.transcript_replayed = true;
                state.transcript = entries;
                state.transcript_revision += 1;
            });
            this.sync(cx);
        }

        fn prompt(id: &str) -> SessionMessageEntry {
            let mut entry = assistant(
                id,
                MessageStatus::Complete,
                vec![text_part("text", "Please explain this.")],
            );
            entry.role = MessageRole::User;
            entry
        }

        fn tick(
            transcript: &Entity<Transcript>,
            window: gpui::WindowHandle<Transcript>,
            cx: &mut gpui::App,
        ) {
            transcript.update(cx, |this, cx| {
                this.own_turn_last_tick = Some(Instant::now() - Duration::from_millis(17));
                this.spring_last_tick = Some(Instant::now() - Duration::from_millis(17));
                if this.own_turn.is_some() {
                    this.step_own_turn(cx);
                }
                if this.pinned {
                    this.step_spring(cx);
                }
            });
            draw(window, cx);
        }

        fn wheel(window: gpui::WindowHandle<Transcript>, delta: f32, cx: &mut gpui::App) {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                        position: gpui::point(px(500.0), px(400.0)),
                        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(delta))),
                        ..Default::default()
                    }),
                    cx,
                );
            })
            .unwrap();
        }

        #[test]
        fn runway_first_echo_starts_a_glide_in_an_empty_chat() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(this, vec![], cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx); // notification before the optimistic echo
                transcript.update(cx, |this, cx| feed(this, vec![prompt("prompt")], cx));
                draw(window, cx);
                let mut previous = transcript.read(cx).list.bounds_for_item(0).unwrap().top();
                assert!(
                    previous > px(400.0),
                    "first echo skipped its glide: {previous:?}"
                );
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                    let top = transcript.read(cx).list.bounds_for_item(0).unwrap().top();
                    assert!(top <= previous + px(0.5));
                    assert!(previous - top < px(150.0));
                    previous = top;
                }
                assert!(previous.abs() <= px(1.0));
            });
        }

        #[test]
        fn runway_real_stream_consumes_reservation_then_follows_until_completion() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(this, vec![prompt("prompt")], cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                let mut text = String::new();
                let mut handed_off = false;
                for chunk in 0..30 {
                    text.push_str(&format!("\n\nSection {chunk}. A paragraph to explain the result.\n\n```text\ncontent {chunk}\n```\n"));
                    transcript.update(cx, |this, cx| {
                        feed(
                            this,
                            vec![
                                prompt("prompt"),
                                assistant(
                                    "reply",
                                    MessageStatus::Streaming,
                                    vec![text_part("text", &text)],
                                ),
                            ],
                            cx,
                        )
                    });
                    draw(window, cx); // assert the first paint, before correction
                    let this = transcript.read(cx);
                    if this.own_turn.is_some() && !this.list.tail_reservation_filled() {
                        assert!(
                            this.list.max_offset_for_scrollbar().y <= px(2.5),
                            "provisional blank space after chunk {chunk}"
                        );
                    }
                    for _ in 0..50 {
                        tick(&transcript, window, cx);
                    }
                    let this = transcript.read(cx);
                    if this.own_turn.is_none() {
                        handed_off = true;
                        assert!(this.pinned, "overflow lost automatic following");
                        assert!(
                            this.distance_from_bottom() <= 1.0,
                            "stream stopped following at chunk {chunk}"
                        );
                    }
                }
                assert!(handed_off, "long output never retired the runway");
                transcript.update(cx, |this, cx| {
                    feed(
                        this,
                        vec![
                            prompt("prompt"),
                            assistant(
                                "reply",
                                MessageStatus::Complete,
                                vec![text_part("text", &text)],
                            ),
                        ],
                        cx,
                    )
                });
                draw(window, cx);
                for _ in 0..50 {
                    tick(&transcript, window, cx);
                }
                assert!(transcript.read(cx).distance_from_bottom() <= 1.0);
            });
        }

        #[test]
        fn runway_wheel_down_cannot_enter_a_temporary_gap_or_reverse() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = vec![viewport_row("prompt", "prompt")];
                    this.list.reset(1);
                    this.rail_enabled = false;
                    this.on_own_send("chat".into(), "prompt".into(), cx);
                    this.list.scroll_to(ListOffset::default());
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| append_runway_rows(this, 3, cx));
                draw(window, cx);
                let mut previous = px(0.0);
                for _ in 0..20 {
                    wheel(window, -60.0, cx);
                    assert!(
                        !transcript.read(cx).own_turn.as_ref().unwrap().held,
                        "real wheel event must release the hold"
                    );
                    tick(&transcript, window, cx);
                    let this = transcript.read(cx);
                    let top = this
                        .list
                        .bounds_for_item(0)
                        .map(|bounds| bounds.top())
                        .unwrap_or_else(|| {
                            // A genuine bottom pin uses GPUI's end sentinel, for
                            // which bounds_for_item intentionally returns None.
                            this.list.viewport_bounds().top()
                                + this.list.offset_for_item(0)
                                + this.list.scroll_px_offset_for_scrollbar().y
                        });
                    assert!(top >= px(-2.5), "wheel entered a blank runway: {top:?}");
                    assert!(
                        top <= previous + px(0.5),
                        "downward wheel reversed: {previous:?} -> {top:?}"
                    );
                    previous = top;
                }
            });
        }

        #[test]
        fn runway_background_burst_and_downward_input_reach_the_new_tail() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(this, vec![prompt("prompt")], cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                // Apply many commits with no layouts or animation frames,
                // just as when the native window has stopped requesting them.
                let mut text = String::new();
                for chunk in 0..40 {
                    text.push_str(&format!(
                        "\n\nSection {chunk}\n\n```text\nresult {chunk}\n```\n"
                    ));
                    transcript.update(cx, |this, cx| {
                        feed(
                            this,
                            vec![
                                prompt("prompt"),
                                assistant(
                                    "reply",
                                    MessageStatus::Streaming,
                                    vec![text_part("text", &text)],
                                ),
                            ],
                            cx,
                        )
                    });
                }
                draw(window, cx);
                let mut previous = -transcript.read(cx).list.scroll_px_offset_for_scrollbar().y;
                for _ in 0..100 {
                    wheel(window, -180.0, cx);
                    tick(&transcript, window, cx);
                    let this = transcript.read(cx);
                    let current = -this.list.scroll_px_offset_for_scrollbar().y;
                    assert!(
                        current >= previous - px(1.0),
                        "refocus wheel snapped backward: {previous:?} -> {current:?}"
                    );
                    previous = current;
                }
                let this = transcript.read(cx);
                assert!(this.own_turn.is_none());
                assert!(
                    this.distance_from_bottom() <= 1.0,
                    "downward input never reached new output"
                );
                assert!(previous > px(800.0));
            });
        }

        #[test]
        fn runway_second_send_and_steer_keep_the_previous_viewport_until_echo() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(this, vec![prompt("first")], cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "first".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                let mut entries = vec![prompt("first")];
                for (ix, status) in [MessageStatus::Complete, MessageStatus::Streaming]
                    .into_iter()
                    .enumerate()
                {
                    entries.push(assistant(
                        &format!("reply-{ix}"),
                        status,
                        vec![text_part("text", "A short answer.")],
                    ));
                    transcript.update(cx, |this, cx| feed(this, entries.clone(), cx));
                    draw(window, cx);
                    for _ in 0..10 {
                        tick(&transcript, window, cx);
                    }
                    let before = transcript.read(cx).list.scroll_px_offset_for_scrollbar().y;
                    let id = format!("next-{ix}");
                    transcript.update(cx, |this, cx| {
                        this.on_own_send("chat".into(), id.clone(), cx)
                    });
                    draw(window, cx); // the echoed prompt has not landed yet
                    assert!(
                        (transcript.read(cx).list.scroll_px_offset_for_scrollbar().y - before)
                            .abs()
                            <= px(1.0),
                        "waiting for echo moved the viewport"
                    );
                    entries.push(prompt(&id));
                    transcript.update(cx, |this, cx| feed(this, entries.clone(), cx));
                    draw(window, cx);
                    let anchor = transcript.read(cx).own_turn_anchor_ix().unwrap();
                    let mut previous = transcript
                        .read(cx)
                        .list
                        .bounds_for_item(anchor)
                        .unwrap()
                        .top();
                    assert!(previous > px(Transcript::own_send_inset(anchor) + 40.0));
                    for _ in 0..80 {
                        tick(&transcript, window, cx);
                        let top = transcript
                            .read(cx)
                            .list
                            .bounds_for_item(anchor)
                            .unwrap()
                            .top();
                        assert!(top <= previous + px(0.5), "repeat send reversed");
                        assert!(
                            top >= px(Transcript::own_send_inset(anchor) - 2.5),
                            "repeat send overshot"
                        );
                        assert!(previous - top < px(150.0), "repeat send jumped");
                        previous = top;
                    }
                }
            });
        }

        #[test]
        fn runway_user_scroll_up_stays_released_when_output_overflows() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    let mut entries = vec![prompt("old")];
                    entries.push(assistant(
                        "history",
                        MessageStatus::Complete,
                        vec![text_part("text", &"History paragraph.\n\n".repeat(80))],
                    ));
                    entries.push(prompt("prompt"));
                    feed(this, entries, cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                wheel(window, 300.0, cx);
                draw(window, cx);
                assert!(!transcript.read(cx).own_turn.as_ref().unwrap().held);
                let before = transcript.read(cx).list.logical_scroll_top();
                transcript.update(cx, |this, cx| {
                    let mut entries = this.state.read(cx).transcript.clone();
                    entries.push(assistant(
                        "reply",
                        MessageStatus::Streaming,
                        vec![text_part("text", &"Long streamed reply.\n\n".repeat(80))],
                    ));
                    feed(this, entries, cx);
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                let this = transcript.read(cx);
                assert!(!this.pinned, "background growth stole the user's viewport");
                let after = this.list.logical_scroll_top();
                assert_eq!(before.item_ix, after.item_ix);
                assert!((before.offset_in_item - after.offset_in_item).abs() <= px(1.0));
            });
        }

        #[test]
        fn runway_resizes_in_the_same_layout_without_a_provisional_gap() {
            struct SizedTranscript {
                transcript: Entity<Transcript>,
                height: f32,
            }
            impl Render for SizedTranscript {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    div()
                        .w_full()
                        .h(px(self.height))
                        .child(self.transcript.clone())
                }
            }
            with_window(|transcript, _, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = vec![viewport_row("prompt", "prompt")];
                    this.list.reset(1);
                    this.rail_enabled = false;
                    this.on_own_send("chat".into(), "prompt".into(), cx);
                    this.list.scroll_to(ListOffset::default());
                });
                let window = cx
                    .open_window(gpui::WindowOptions::default(), |_, cx| {
                        cx.new(|_| SizedTranscript {
                            transcript: transcript.clone(),
                            height: 600.0,
                        })
                    })
                    .unwrap();
                for height in [600.0, 900.0, 450.0, 800.0] {
                    window
                        .update(cx, |root, window, cx| {
                            root.height = height;
                            cx.notify();
                            window.refresh();
                        })
                        .unwrap();
                    cx.update_window(window.into(), |_, window, cx| {
                        let _ = window.draw(cx);
                    })
                    .unwrap();
                    let this = transcript.read(cx);
                    assert_eq!(this.list.viewport_bounds().size.height, px(height));
                    assert!(
                        (this.list.max_offset_for_scrollbar().y - px(2.0)).abs() <= px(0.5),
                        "resize exposed blank space"
                    );
                    assert!(this.list.bounds_for_item(0).unwrap().top().abs() <= px(0.5));
                }
            });
        }

        #[test]
        fn runway_direct_jump_to_an_unmeasured_tail_does_not_reserve_unknown_rows() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = (0..100)
                        .map(|ix| viewport_row(&format!("row-{ix}"), &format!("entry-{ix}")))
                        .collect();
                    this.list.reset(100);
                    this.list.scroll_to(ListOffset {
                        item_ix: 99,
                        offset_in_item: px(0.0),
                    });
                    this.pinned = false;
                    this.rail_enabled = false;
                    cx.notify();
                });
                draw(window, cx);
                let natural = transcript.read(cx).list.offset_for_item(100)
                    - transcript.read(cx).list.offset_for_item(99);
                transcript.update(cx, |this, cx| {
                    this.list.reset(100); // discard all prefix height hints
                    this.on_own_send("chat".into(), "entry-0".into(), cx);
                    this.release_own_turn_hold();
                    this.list.scroll_to(ListOffset {
                        item_ix: 99,
                        offset_in_item: px(0.0),
                    });
                });
                draw(window, cx);
                let this = transcript.read(cx);
                assert_eq!(
                    this.list.offset_for_item(100) - this.list.offset_for_item(99),
                    natural,
                    "unknown prefix rows created a blank tail"
                );
                assert!(this.distance_from_bottom() <= 1.0);
            });
        }

        #[test]
        fn runway_wheel_down_at_the_end_keeps_following_when_streaming_overflows() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(this, vec![prompt("prompt")], cx);
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                wheel(window, -60.0, cx);
                // Let the real wheel handler's deferred ListState read finish
                // before delivering more output, as in the native event loop.
                cx.defer(move |cx| {
                    assert!(
                        transcript.read(cx).pinned,
                        "downward input at the runway end must retain follow intent"
                    );
                    let mut text = String::new();
                    for chunk in 0..20 {
                        text.push_str(&format!(
                            "\n\nSection {chunk}\n\n```text\ncontent {chunk}\n```\n"
                        ));
                        transcript.update(cx, |this, cx| {
                            feed(
                                this,
                                vec![
                                    prompt("prompt"),
                                    assistant(
                                        "reply",
                                        MessageStatus::Streaming,
                                        vec![text_part("text", &text)],
                                    ),
                                ],
                                cx,
                            )
                        });
                        draw(window, cx);
                        for _ in 0..40 {
                            tick(&transcript, window, cx);
                        }
                    }
                    let this = transcript.read(cx);
                    assert!(this.own_turn.is_none());
                    assert!(this.pinned);
                    assert!(
                        this.distance_from_bottom() <= 1.0,
                        "output stopped following after the runway filled"
                    );
                });
            });
        }

        #[test]
        fn runway_down_then_up_before_overflow_preserves_the_user_viewport() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    feed(
                        this,
                        vec![
                            prompt("old"),
                            assistant(
                                "history",
                                MessageStatus::Complete,
                                vec![text_part("text", &"History.\n\n".repeat(80))],
                            ),
                            prompt("prompt"),
                        ],
                        cx,
                    );
                    this.rail_enabled = false;
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                for _ in 0..80 {
                    tick(&transcript, window, cx);
                }
                wheel(window, -60.0, cx);
                cx.defer(move |cx| {
                    assert!(transcript.read(cx).pinned);
                    draw(window, cx);
                    wheel(window, 300.0, cx);
                    assert!(
                        !transcript.read(cx).pinned,
                        "upward input must cancel the spring synchronously"
                    );
                    cx.defer(move |cx| {
                        assert!(!transcript.read(cx).pinned);
                        let before = transcript.read(cx).list.logical_scroll_top();
                        transcript.update(cx, |this, cx| {
                            let mut entries = this.state.read(cx).transcript.clone();
                            entries.push(assistant(
                                "reply",
                                MessageStatus::Streaming,
                                vec![text_part("text", &"New output.\n\n".repeat(80))],
                            ));
                            feed(this, entries, cx);
                        });
                        draw(window, cx);
                        for _ in 0..80 {
                            tick(&transcript, window, cx);
                        }
                        let this = transcript.read(cx);
                        assert!(!this.pinned);
                        let after = this.list.logical_scroll_top();
                        assert_eq!(before.item_ix, after.item_ix);
                        assert!((before.offset_in_item - after.offset_in_item).abs() <= px(1.0));
                    });
                });
            });
        }

        #[test]
        fn runway_absorbs_tail_shrinkage_in_the_same_layout() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    let mut row = viewport_row("prompt", "prompt");
                    row.kind = RowKind::ErrorChip {
                        message: "line\n".repeat(12).into(),
                    };
                    this.rows = vec![row];
                    this.list.reset(1);
                    this.pinned = false;
                    this.rail_enabled = false;
                    cx.notify();
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "prompt".into(), cx)
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.list.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.0),
                    });
                    this.step_own_turn(cx);
                });
                draw(window, cx);
                let before = transcript.read(cx).list.bounds_for_item(0).unwrap();
                transcript.update(cx, |this, cx| {
                    // Simulate completion removing content from the last row.
                    this.rows[0].kind = RowKind::ErrorChip {
                        message: "done".into(),
                    };
                    this.list.remeasure_items(0..1);
                    cx.notify();
                });
                // No controller tick between the change and this paint.
                draw(window, cx);
                let after = transcript.read(cx).list.bounds_for_item(0).unwrap();
                assert_eq!(
                    before.top(),
                    after.top(),
                    "completion must not move the prompt"
                );
                assert_eq!(
                    before.size.height, after.size.height,
                    "the runway absorbs the shrink"
                );
                transcript.update(cx, |this, cx| {
                    this.rows[0].kind = RowKind::ErrorChip {
                        message: "line\n".repeat(100).into(),
                    };
                    this.list.remeasure_items(0..1);
                    cx.notify();
                });
                draw(window, cx);
                let overflow_height = transcript
                    .read(cx)
                    .list
                    .bounds_for_item(0)
                    .unwrap()
                    .size
                    .height;
                transcript.update(cx, |this, cx| {
                    this.step_own_turn(cx);
                    assert!(
                        this.own_turn.is_none(),
                        "overflow must retire the reservation"
                    );
                    assert!(this.pinned, "a held turn hands off to tail-follow");
                });
                draw(window, cx);
                assert_eq!(
                    transcript
                        .read(cx)
                        .list
                        .bounds_for_item(0)
                        .unwrap()
                        .size
                        .height,
                    overflow_height,
                    "retiring the minimum must be height-neutral"
                );
            });
        }

        #[test]
        fn send_glide_never_crosses_the_prompt_during_remeasurement() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = (0..12)
                        .map(|ix| viewport_row(&format!("row-{ix}"), &format!("entry-{ix}")))
                        .collect();
                    this.list.reset(this.rows.len());
                    this.rail_enabled = false;
                    cx.notify();
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    this.on_own_send("chat".into(), "entry-11".into(), cx)
                });
                draw(window, cx);
                let start_top = transcript.read(cx).list.bounds_for_item(11).unwrap().top();
                assert!(
                    start_top > px(Transcript::own_send_inset(11) + 100.0),
                    "installing the runway must preserve the start of the glide"
                );
                let mut previous_top = start_top;
                for _ in 0..90 {
                    draw(window, cx);
                    transcript.update(cx, |this, cx| {
                        // Pending-echo changes can invalidate the prompt before
                        // the queued glide runs. Exercise that exact ordering.
                        this.remeasure_last_row();
                        this.own_turn_last_tick = Some(Instant::now() - Duration::from_millis(17));
                        this.step_own_turn(cx);
                    });
                    draw(window, cx);
                    let this = transcript.read(cx);
                    let bounds = this.list.bounds_for_item(11).unwrap();
                    assert!(
                        bounds.top() <= previous_top + px(0.5),
                        "the glide must not reverse"
                    );
                    previous_top = bounds.top();
                    let target =
                        this.list.viewport_bounds().top() + px(Transcript::own_send_inset(11));
                    assert!(
                        bounds.top() >= target - px(0.5),
                        "send overshot: {:?} < {:?}",
                        bounds.top(),
                        target
                    );
                }
                let this = transcript.read(cx);
                let bounds = this.list.bounds_for_item(11).unwrap();
                assert!(
                    (f32::from(bounds.top() - this.list.viewport_bounds().top())
                        - Transcript::own_send_inset(11))
                    .abs()
                        <= 1.0
                );
            });
        }

        #[test]
        fn background_overflow_retires_hold_before_the_tail_is_measured() {
            with_window(|transcript, window, cx| {
                transcript.update(cx, |this, cx| {
                    this.rows = (0..100)
                        .map(|ix| viewport_row(&format!("row-{ix}"), &format!("entry-{ix}")))
                        .collect();
                    this.list.reset(this.rows.len());
                    this.list.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.0),
                    });
                    this.pinned = false;
                    this.rail_enabled = false;
                    this.own_turn = Some(OwnTurnAnchor {
                        chat_id: "chat".into(),
                        message_id: "entry-0".into(),
                        held: true,
                        positioned: true,
                        seen_prompt: true,
                    });
                    cx.notify();
                });
                draw(window, cx);
                transcript.update(cx, |this, cx| {
                    assert!(
                        this.list.bounds_for_item(99).is_none(),
                        "tail must remain virtualized"
                    );
                    this.step_own_turn(cx);
                    assert!(
                        this.own_turn.is_none(),
                        "a filled hold cannot wait on off-screen bounds"
                    );
                    assert!(this.pinned);
                });
            });
        }

        #[test]
        fn wheel_down_releases_stale_hold_before_a_background_frame_can_run() {
            with_transcript(|this, cx| {
                this.rows = vec![
                    viewport_row("prompt", "prompt"),
                    viewport_row("reply", "reply"),
                ];
                this.list.reset(2);
                this.own_turn = Some(OwnTurnAnchor {
                    chat_id: "chat".into(),
                    message_id: "prompt".into(),
                    held: true,
                    positioned: true,
                    seen_prompt: true,
                });
                this.own_turn_scheduled = true;
                // A downward scroll into output received while no layout/frame
                // callbacks were running. No preceding upward gesture.
                this.list.scroll_to(ListOffset {
                    item_ix: 1,
                    offset_in_item: px(20.0),
                });
                this.handle_scroll(
                    &ListScrollEvent {
                        visible_range: 1..2,
                        count: 2,
                        is_scrolled: true,
                        is_following_tail: false,
                    },
                    cx,
                );
                assert!(
                    !this.own_turn.as_ref().unwrap().held,
                    "input must cancel the queued hold synchronously"
                );
                let entity = cx.entity();
                cx.defer(move |cx| {
                    let this = entity.read(cx);
                    assert!(!this.own_turn.as_ref().unwrap().held);
                    assert_eq!(this.list.logical_scroll_top().item_ix, 1);
                    assert_eq!(this.list.logical_scroll_top().offset_in_item, px(20.0));
                });
            });
        }

        #[test]
        fn folding_releases_sent_turn_hold_without_removing_reservation() {
            with_transcript(|transcript, _| {
                for reduced_motion in [false, true] {
                    for open in [false, true] {
                        transcript.own_turn = Some(OwnTurnAnchor {
                            chat_id: "chat".into(),
                            message_id: "prompt".into(),
                            held: true,
                            positioned: true,
                            seen_prompt: true,
                        });
                        transcript.pinned = true;
                        transcript.spring_kick = true;
                        transcript.own_turn_last_tick = Some(Instant::now());
                        transcript
                            .user_folds
                            .entry("prompt".into())
                            .or_default()
                            .open = Some(open);

                        // No row bounds yet: ownership must transfer even if
                        // geometry is unavailable during a list remeasurement.
                        transcript.toggle_user_fold(
                            "prompt".into(),
                            0,
                            110.0,
                            2200.0,
                            reduced_motion,
                        );

                        let turn = transcript.own_turn.as_ref().unwrap();
                        assert!(
                            !turn.held,
                            "an outgrown reservation must not re-engage the pin"
                        );
                        assert!(transcript.own_turn_last_tick.is_none());
                        assert!(!transcript.pinned);
                        assert!(!transcript.spring_kick);
                        assert_eq!(transcript.user_folds["prompt"].open, Some(!open));
                    }
                }
            });
        }

        #[test]
        fn navigation_cancels_fold_compensation_before_queued_frame() {
            with_transcript(|transcript, cx| {
                for navigation in ["wheel", "rail", "bottom", "send"] {
                    transcript.user_collapse_scroll = Some(UserCollapseScroll {
                        started_at: Instant::now(),
                        duration_ms: 850,
                        height_delta: 2000.0,
                        row_ix: 0,
                        initial_top: -1000.0,
                        target_top: 80.0,
                    });
                    transcript.user_collapse_scroll_scheduled = true;
                    let hold_token = transcript.user_hold_token;
                    match navigation {
                        "wheel" => transcript.handle_scroll(
                            &ListScrollEvent {
                                visible_range: 0..0,
                                count: 0,
                                is_scrolled: true,
                                is_following_tail: false,
                            },
                            cx,
                        ),
                        "rail" => transcript.begin_scroll_navigation(),
                        "bottom" => transcript.jump_to_bottom(cx),
                        "send" => transcript.on_own_send("chat".into(), "prompt".into(), cx),
                        _ => unreachable!(),
                    }
                    assert!(transcript.user_collapse_scroll.is_none(), "{navigation}");
                    assert_ne!(
                        transcript.user_hold_token, hold_token,
                        "cancel stale long presses"
                    );
                    assert!(
                        transcript.user_collapse_scroll_scheduled,
                        "keep the queued-frame guard"
                    );

                    // A frame queued before the input must neither move the
                    // viewport nor resurrect the canceled compensation.
                    let offset = transcript.list.logical_scroll_top();
                    transcript.user_collapse_scroll_scheduled = false;
                    transcript.step_user_collapse_scroll(cx);
                    let after = transcript.list.logical_scroll_top();
                    assert_eq!(after.item_ix, offset.item_ix);
                    assert_eq!(after.offset_in_item, offset.offset_in_item);
                    assert!(transcript.user_collapse_scroll.is_none());
                }
            });
        }
    }

    /// Explicit multiline and long soft-wrapped prompts get a fold affordance;
    /// short messages stay untouched.
    #[test]
    fn long_prompts_collapse_and_short_ones_do_not() {
        assert!(!user_message_needs_collapse("short message"));
        assert!(!user_message_needs_collapse("1\n2\n3\n4\n5"));
        assert!(user_message_needs_collapse("1\n2\n3\n4\n5\n6"));
        assert!(
            !user_message_needs_collapse(&"x".repeat(240)),
            "ordinary two- or three-line prose must not grow a toggle"
        );
        assert!(!user_message_needs_collapse(
            &"x".repeat(USER_COLLAPSE_CHARS)
        ));
        assert!(user_message_needs_collapse(
            &"x".repeat(USER_COLLAPSE_CHARS + 1)
        ));
    }

    #[test]
    fn user_resize_duration_scales_with_distance_and_stays_bounded() {
        let short = user_resize_duration_ms(100.0);
        let medium = user_resize_duration_ms(600.0);
        let long = user_resize_duration_ms(2_000.0);
        assert!((220..=260).contains(&short));
        assert!(medium > short);
        assert_eq!(long, 850);
        assert_eq!(
            user_resize_spec(100.0).curve,
            motion::EASE_OUT,
            "short folds keep the decisive sidebar-like ease-out"
        );
        assert_eq!(
            user_resize_spec(2_000.0).curve,
            motion::EASE_IN_OUT,
            "large folds avoid front-loading the whole travel"
        );
    }

    /// The toggle is render-local: expanding a prompt must not change the
    /// row's identity or version, or the list would splice (and the
    /// virtualizer would drop the scroll anchor) on every click.
    #[test]
    fn expanding_a_prompt_is_not_a_row_change() {
        let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &"a line\n".repeat(40))];
        let before = rows_for_entry(&entry, false, &mut parse);
        let after = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].id, after[0].id);
        assert_eq!(before[0].version, after[0].version);
    }

    #[test]
    fn user_rows_split_attachment_refs_from_text() {
        let content = crate::attachments::with_attachments(
            "what color is this?",
            &["/data/uploads/ab12-red.png".to_string()],
        );
        let mut entry = assistant("u2", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &content)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "what color is this?");
        assert_eq!(rows[0].copy_text.as_deref(), Some("what color is this?"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, "/data/uploads/ab12-red.png");
        assert_eq!(attachments[0].name, "ab12-red.png");

        // Image-only send: no bubble text, refs parsed.
        let only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
        entry.parts = vec![text_part("t0", &only)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "");
        assert!(rows[0].copy_text.is_none());
        assert_eq!(attachments.len(), 1);
    }

    /// A sent prompt's file mentions render as chips in the transcript: the
    /// row carries the projected display text plus spans, while ordinary
    /// prompts keep the empty-spans fast path. The row version derives from
    /// the RAW text either way, so projection never perturbs the diff key.
    #[test]
    fn user_rows_project_file_mentions_into_chips() {
        let raw = "look at [composer.rs](zeron-file:crates/ui/src/composer.rs) please";
        let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", raw)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert!(
            !text.contains("zeron-file:"),
            "raw link left visible: {text}"
        );
        assert!(text.contains("composer.rs"));
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].is_dir);
        assert_eq!(mentions[0].path.as_ref(), "crates/ui/src/composer.rs");
        assert_eq!(&text[mentions[0].range.clone()], {
            let projected: &str = "\u{00A0}@composer.rs\u{00A0}";
            projected
        });
        assert_eq!(rows[0].version, (raw.len() as u64) << 1);

        entry.parts = vec![text_part("t0", "no mentions here")];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "no mentions here");
        assert!(mentions.is_empty());
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
        // Same ids; every version flips its streaming bit → one 3-row splice.
        assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..3, 3)));
    }

    #[test]
    fn tool_diff_builds_real_hunks_with_context_and_numbers() {
        use crate::changes::LineKind;
        let old = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let mut new = old.clone();
        new[9] = "LINE 10".into();
        let diff = zeron_proto::ToolDiff {
            path: "/w/a.rs".into(),
            old_text: Some(old.join("\n") + "\n"),
            new_text: new.join("\n") + "\n",
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&diff), None)
        else {
            panic!("expected diff detail");
        };
        // One hunk: the change plus 3 context lines each side, real numbers.
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -7,7 +7,7 @@");
        assert_eq!(hunk.lines.len(), 8); // 6 context + 1 del + 1 add
        let del = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Del)
            .expect("del line");
        assert_eq!(del.old_no, Some(10));
        assert_eq!(del.new_no, None);
        assert_eq!(del.text, "line 10");
        let add = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Add)
            .expect("add line");
        assert_eq!(add.new_no, Some(10));
        assert_eq!(add.text, "LINE 10");
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(old_text.as_deref(), diff.old_text.as_deref());
        assert_eq!(new_text.as_deref(), Some(diff.new_text.as_str()));
        // New files carry Added status (and no old numbers).
        let created = zeron_proto::ToolDiff {
            path: "/w/new.txt".into(),
            old_text: None,
            new_text: "only\n".into(),
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&created), None)
        else {
            panic!("expected diff detail");
        };
        assert_eq!(file.status, crate::changes::FileStatus::Added);
        assert!(old_text.is_none());
        assert_eq!(new_text.as_deref(), Some("only\n"));

        // Output: verbatim lines (indentation intact), counted-tail cap.
        let output = (0..40)
            .map(|i| format!("    indented {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = tool_detail(Some(&output), None, None)
        else {
            panic!("expected output detail");
        };
        assert_eq!(lines.len(), OUTPUT_DETAIL_MAX_LINES);
        assert_eq!(truncated_by, 40 - OUTPUT_DETAIL_MAX_LINES);
        assert_eq!(lines[0].as_ref(), "    indented 0");

        // Nothing → no affordance.
        assert!(tool_detail(None, None, None).is_none());
        assert!(tool_detail(Some("\n\n"), None, None).is_none());
    }

    #[test]
    fn tool_group_summaries() {
        let exec = |c: &str| ToolItem {
            call: ToolCall::Exec { command: c.into() },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
        };
        let edit = |p: &str| ToolItem {
            call: ToolCall::EditFile {
                path: p.into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
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
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
            ToolItem {
                call: ToolCall::Glob {
                    pattern: "*.rs".into(),
                },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
            ToolItem {
                call: ToolCall::WebSearch { query: "q".into() },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
        ];
        assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
    }

    #[test]
    fn subagent_tab_titles() {
        // The tab is the BARE task — the "Agent:" genus is stripped.
        let named = ToolCall::Unknown {
            name: "Agent: scan repo".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&named).as_ref(), "scan repo");
        // A bare "Task"/"Agent" digs the description out of the call input
        // (which sheds any genus of its own).
        let bare = ToolCall::Unknown {
            name: "Task".into(),
            input: Some(serde_json::json!({
                "description": "Agent: audit the auth flow",
                "prompt": "very long instructions…",
            })),
        };
        assert_eq!(subagent_tab_title(&bare).as_ref(), "audit the auth flow");
        // Word boundaries only — a name that merely STARTS with the genus
        // keeps itself.
        let compound = ToolCall::Unknown {
            name: "Taskmaster".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&compound).as_ref(), "Taskmaster");
        // Nothing to derive → the generic label.
        let blank = ToolCall::Unknown {
            name: "agent".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&blank).as_ref(), "Subagent");
        // Absurd lengths cap with an ellipsis; multiline prompts keep only
        // their first line.
        let long = ToolCall::Unknown {
            name: "x".repeat(120),
            input: None,
        };
        let title = subagent_tab_title(&long);
        assert_eq!(title.chars().count(), SUBAGENT_TITLE_MAX + 1);
        assert!(title.ends_with('…'));
        // Non-spawn-shaped calls stay generic.
        assert_eq!(
            subagent_tab_title(&ToolCall::Exec {
                command: "ls".into()
            })
            .as_ref(),
            "Subagent"
        );
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
                zeron_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                zeron_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
    }

    #[test]
    fn multiline_command_flattens_to_one_chip_line() {
        // The user's breaker: a multi-line script in a Run chip. The detail
        // must come out as ONE sanitized line — the chip's fixed 30px card
        // then truncates it with an ellipsis like the original's CSS.
        let (label, detail) = tool_chip_content(&ToolCall::Exec {
            command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
        });
        assert_eq!(label, "Run");
        assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
        assert!(!detail.contains('\n'));
        // The chip row height is a constant, independent of content shape.
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        // Every detail kind is sanitized (MCP inputs / queries are model text).
        let (_, q) = tool_chip_content(&ToolCall::WebSearch {
            query: "line one\nline two".into(),
        });
        assert_eq!(q, "line one line two");
    }

    #[test]
    fn call_block_carries_the_full_invocation() {
        // Multi-line command: verbatim lines, not the flattened chip line.
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = call_block(&ToolCall::Exec {
            command: "set -e\ncargo test".into(),
        })
        else {
            panic!("expected an output block")
        };
        assert_eq!(truncated_by, 0);
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["set -e", "cargo test"]
        );

        // A long single-line command soft-wraps instead of ellipsizing.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Exec {
            command: "x".repeat(CALL_WRAP_COLS * 2 + 10),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.chars().count() <= CALL_WRAP_COLS));

        // MCP input pretty-prints under the `server · tool` line.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Mcp {
            server: "gh".into(),
            tool: "issues".into(),
            input: Some(serde_json::json!({"repo": "zeron"})),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines[0].as_ref(), "gh · issues");
        assert!(lines.iter().any(|l| l.contains("\"repo\": \"zeron\"")));

        // Todos list one item per line with checkbox state.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Todo {
            items: vec![
                zeron_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                zeron_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["[x] a", "[ ] b"]
        );

        // Blank invocation → no block; the chip stays a plain card.
        assert!(
            call_block(&ToolCall::Exec {
                command: "  \n ".into()
            })
            .is_none()
        );
    }

    #[test]
    fn timestamp_strip_lands_on_the_last_settled_row() {
        use chrono::FixedOffset;
        // Fixed zone (UTC−4): "Jul 1, 3:45 PM" — the exact formatTimestamp
        // shape (short month, numeric day, no leading zero, 2-digit minutes).
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let ms = chrono::DateTime::parse_from_rfc3339("2026-07-01T19:45:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(format_timestamp(ms, &tz), "Jul 1, 3:45 PM");

        // User entries carry the strip on their single row (pending too).
        let user = SessionMessageEntry {
            id: "u1".into(),
            role: MessageRole::User,
            parts: vec![text_part("p1", "hi")],
            created_at: ms,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        };
        let rows = rows_for_entry(&user, true, &mut parse);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, Some(ms));

        // Assistant entries: strip on the LAST row once settled…
        let done = assistant(
            "a1",
            MessageStatus::Complete,
            vec![text_part("p1", "one\n\ntwo")],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        assert!(rows.len() >= 2);
        assert_eq!(rows.last().unwrap().timestamp, Some(done.created_at));
        assert_eq!(
            rows.last().unwrap().copy_text.as_deref(),
            Some("one\n\ntwo")
        );
        assert!(rows[..rows.len() - 1].iter().all(|r| r.timestamp.is_none()));
        assert!(rows[..rows.len() - 1].iter().all(|r| r.copy_text.is_none()));

        // …but never mid-stream (chat-view.tsx: no hover under a moving reply).
        let live = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![text_part("p1", "streaming…")],
        );
        let rows = rows_for_entry(&live, false, &mut parse);
        assert!(rows.iter().all(|r| r.timestamp.is_none()));
        assert!(rows.iter().all(|r| r.copy_text.is_none()));
        // Every row knows its entry (the hover group).
        assert!(rows.iter().all(|r| r.entry_id.as_ref() == live.id));
    }

    #[test]
    fn message_copy_keeps_authored_text_and_excludes_tool_traces() {
        let entry = assistant(
            "a-copy",
            MessageStatus::Complete,
            vec![
                text_part("p1", "First **paragraph**."),
                tool_part("tool", "printf hidden"),
                text_part("p2", "    indented code\n    stays indented"),
            ],
        );
        assert_eq!(
            assistant_copy_text(&entry).as_deref(),
            Some("First **paragraph**.\n\n    indented code\n    stays indented")
        );
    }

    #[test]
    fn single_line_collapses_all_whitespace_runs() {
        assert_eq!(single_line("a\nb"), "a b");
        assert_eq!(single_line("  a\t\t b \r\n c  "), "a b c");
        assert_eq!(single_line("plain"), "plain");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("\n\n"), "");
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
    fn sending_bridge_holds_until_the_turn_outdates_the_send() {
        let send = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .unwrap()
            .to_utc();
        let before = send - chrono::Duration::seconds(90);
        let after = send + chrono::Duration::seconds(2);
        // In flight, row still on the previous turn (or no row yet).
        assert!(sending_bridge(Some(send), Some(before)));
        assert!(sending_bridge(Some(send), None));
        // The turn started after the send fired — timer takes over.
        assert!(!sending_bridge(Some(send), Some(after)));
        // No send in flight: never a bridge, whatever the row says.
        assert!(!sending_bridge(None, Some(before)));
        assert!(!sending_bridge(None, None));
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
