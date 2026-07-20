//! The composer: a hand-rolled multiline text input (adapted from gpui's
//! `examples/input.rs`), the compact↔expanded flip, the Send/Steer/Stop morph,
//! optimistic send with failure recovery, per-chat drafts, and the question
//! wizard that replaces the composer while a run awaits input.
//!
//! Pure decision logic (flip, auto-grow math, button morph, wizard reducer,
//! pending-input detection) lives in free functions/structs with unit tests;
//! the gpui element only feeds them measurements.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, SharedString, Style, Subscription, Task, TextRun, TextStyle, UTF16Selection,
    UnderlineStyle, Window, WrappedLine, actions, div, fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use comet_doc::{MessagePart, MessageStatus, SessionCommandPayload, SessionMessageEntry};
use comet_proto::{RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion};
use comet_rpc::methods;

use crate::motion;
use crate::pickers::Pickers;
use crate::state::{AppState, Indicator};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Constants + pure decision logic
// ---------------------------------------------------------------------------

/// Expanded composer height bounds: one line at the floor, textarea capped at
/// 260px of content (comet `max-h-[260px]`) plus the chrome.
pub const COMPOSER_MIN_HEIGHT: f32 = INPUT_LINE_HEIGHT + INPUT_VERTICAL_CHROME;
pub const COMPOSER_MAX_HEIGHT: f32 = 260.0 + INPUT_VERTICAL_CHROME;
/// Below this pill input width the composer always expands.
pub const MIN_COMPACT_INPUT_WIDTH: f32 = 200.0;
/// Vertical chrome around the text content in expanded mode: textarea padding
/// (`pt-4 pb-1` = 20) + actions row (`pt-1` 4 + h-8 cluster 32 + `pb-2.5` 10)
/// — comet composer.tsx / composer-actions.tsx.
pub const INPUT_VERTICAL_CHROME: f32 = 66.0;
/// Input text metrics.
pub const INPUT_LINE_HEIGHT: f32 = 21.0;
pub const INPUT_TEXT_SIZE: f32 = 14.0;
/// Single-select questions auto-advance after this long.
pub const AUTO_ADVANCE_MS: u64 = 220;

/// Compact→expanded flip: newline, overflowing text, or a too-narrow pill.
pub fn composer_expanded(text_width: f32, pill_capacity: f32, has_newline: bool) -> bool {
    has_newline || pill_capacity < MIN_COMPACT_INPUT_WIDTH || text_width > pill_capacity
}

/// Auto-grow: content height for a wrapped-line count.
pub fn input_content_height(wrapped_lines: usize) -> f32 {
    wrapped_lines.max(1) as f32 * INPUT_LINE_HEIGHT
}

/// Total expanded composer height for a content height (clamped 76–260).
pub fn composer_total_height(content_height: f32) -> f32 {
    (content_height + INPUT_VERTICAL_CHROME).clamp(COMPOSER_MIN_HEIGHT, COMPOSER_MAX_HEIGHT)
}

/// What the send button is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendButtonMode {
    /// No live run: plain send.
    Send,
    /// Live steerable run with text typed: "Send (steers the current run)".
    Steer,
    /// Live run, nothing typed: red stop square.
    Stop,
}

pub fn send_button_mode(run_live: bool, has_text: bool) -> SendButtonMode {
    match (run_live, has_text) {
        (false, _) => SendButtonMode::Send,
        (true, true) => SendButtonMode::Steer,
        (true, false) => SendButtonMode::Stop,
    }
}

/// Find the live run's unresolved input request, if any: an unresolved input
/// part on a still-streaming entry.
pub fn pending_input_request(
    transcript: &[SessionMessageEntry],
) -> Option<(String, Vec<UserInputQuestion>)> {
    let entry = transcript.last()?;
    if entry.status != Some(MessageStatus::Streaming) {
        return None;
    }
    entry.parts.iter().rev().find_map(|part| match part {
        MessagePart::Input {
            request_id,
            questions,
            resolved: false,
            ..
        } => Some((request_id.clone(), questions.clone())),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Question wizard (pure reducer)
// ---------------------------------------------------------------------------

/// Reducer outcome of a wizard interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Stay,
    /// Single-select landed — advance after [`AUTO_ADVANCE_MS`].
    AutoAdvance,
    /// All pages answered — submit these answers.
    Done(Vec<UserInputAnswer>),
}

/// Paged question state ("1/3"): single-select auto-advances, multi-select and
/// typed answers advance explicitly, number keys 1-9 select, Back pages back.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub page: usize,
    picked: Vec<Vec<usize>>,
    typed: Vec<String>,
}

impl Wizard {
    pub fn new(request_id: String, questions: Vec<UserInputQuestion>) -> Self {
        let n = questions.len();
        Self {
            request_id,
            questions,
            page: 0,
            picked: vec![Vec::new(); n],
            typed: vec![String::new(); n],
        }
    }

    pub fn counter(&self) -> String {
        format!("{}/{}", self.page + 1, self.questions.len().max(1))
    }

    pub fn current(&self) -> Option<&UserInputQuestion> {
        self.questions.get(self.page)
    }

    pub fn is_picked(&self, option_ix: usize) -> bool {
        self.picked
            .get(self.page)
            .is_some_and(|p| p.contains(&option_ix))
    }

    /// Whether the current page has any picked option.
    pub fn page_has_pick(&self) -> bool {
        self.picked.get(self.page).is_some_and(|p| !p.is_empty())
    }

    /// Click/tap an option.
    pub fn select(&mut self, option_ix: usize) -> WizardStep {
        let Some(question) = self.questions.get(self.page) else {
            return WizardStep::Stay;
        };
        if option_ix >= question.options.len() {
            return WizardStep::Stay;
        }
        let multi = question.multi_select;
        let Some(picked) = self.picked.get_mut(self.page) else {
            return WizardStep::Stay;
        };
        if multi {
            match picked.iter().position(|&p| p == option_ix) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option_ix),
            }
            WizardStep::Stay
        } else {
            *picked = vec![option_ix];
            WizardStep::AutoAdvance
        }
    }

    /// Number key 1-9.
    pub fn press_number(&mut self, number: usize) -> WizardStep {
        if number == 0 {
            return WizardStep::Stay;
        }
        self.select(number - 1)
    }

    pub fn set_typed(&mut self, text: String) {
        if let Some(slot) = self.typed.get_mut(self.page) {
            *slot = text;
        }
    }

    /// Explicit submit / auto-advance landing.
    pub fn advance(&mut self) -> WizardStep {
        if self.page + 1 < self.questions.len() {
            self.page += 1;
            WizardStep::Stay
        } else {
            WizardStep::Done(self.answers())
        }
    }

    /// Page back; false when already on the first page.
    pub fn back(&mut self) -> bool {
        if self.page > 0 {
            self.page -= 1;
            true
        } else {
            false
        }
    }

    /// Answers per question: free text overrides picked labels.
    pub fn answers(&self) -> Vec<UserInputAnswer> {
        self.questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let typed = self.typed.get(ix).map(|s| s.trim()).unwrap_or("");
                let labels = if !typed.is_empty() {
                    vec![typed.to_string()]
                } else {
                    self.picked
                        .get(ix)
                        .map(|picked| {
                            picked
                                .iter()
                                .filter_map(|&p| q.options.get(p).cloned())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                UserInputAnswer {
                    question_id: q.id.clone(),
                    labels,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Multiline text input (adapted from gpui examples/input.rs)
// ---------------------------------------------------------------------------

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        WordLeft,
        WordRight,
        Copy,
        Cut,
        Paste,
        Newline,
        Submit,
    ]
);

/// Bind the composer keymap. Call once at app boot.
pub fn init(cx: &mut App) {
    let ctx = Some("Composer");
    let mut bindings = vec![
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
    ];
    // Word navigation and clipboard: bind both modifier conventions so the same
    // map works across platforms.
    for prefix in ["ctrl", "alt"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-left"), WordLeft, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-right"), WordRight, ctx));
    }
    for prefix in ["cmd", "ctrl"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, ctx));
    }
    cx.bind_keys(bindings);
}

/// Events the composer wrapper listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerInputEvent {
    Submitted,
    Edited,
}

/// Multiline input entity: content + selection + IME marked text + measured
/// layout (wrapped lines) for mouse mapping and auto-grow.
pub struct ComposerInput {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    /// Vertical scroll inside the input once content exceeds the max height.
    scroll_top: f32,
    // -- measured state (written during layout/paint) --
    last_lines: Vec<WrappedLine>,
    line_starts: Vec<usize>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    content_height: f32,
    max_line_width: f32,
    last_width: f32,
    display_is_placeholder: bool,
}

impl ComposerInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            scroll_top: 0.0,
            last_lines: Vec::new(),
            line_starts: vec![0],
            last_bounds: None,
            line_height: px(INPUT_LINE_HEIGHT),
            content_height: INPUT_LINE_HEIGHT,
            max_line_width: 0.0,
            last_width: 0.0,
            display_is_placeholder: true,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub fn has_newline(&self) -> bool {
        self.content.contains('\n')
    }

    /// Unwrapped width of the widest line — feeds the compact/expanded flip.
    pub fn measured_text_width(&self) -> f32 {
        self.max_line_width
    }

    pub fn measured_content_height(&self) -> f32 {
        self.content_height
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = 0.0;
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    // ---- editing ops ----

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .rev()
            .find_map(|(ix, word)| (ix < offset && !word.trim().is_empty()).then_some(ix))
            .unwrap_or(0)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .find_map(|(ix, word)| {
                let end = ix + word.len();
                (end > offset && !word.trim().is_empty()).then_some(end)
            })
            .unwrap_or(self.content.len())
    }

    /// Byte range of the logical line containing `offset`.
    fn line_range_at(&self, offset: usize) -> Range<usize> {
        let start = self.content[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len());
        start..end
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.move_to(prev, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.selected_range.end);
            self.move_to(next, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1.0, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1.0, cx);
    }

    fn move_vertical(&mut self, dir: f32, cx: &mut Context<Self>) {
        let Some(current) = self.point_for_index(self.cursor_offset()) else {
            return;
        };
        let target_y = f32::from(current.y) + dir * f32::from(self.line_height);
        if target_y < 0.0 {
            self.move_to(0, cx);
            return;
        }
        if target_y >= self.content_height {
            self.move_to(self.content.len(), cx);
            return;
        }
        let ix = self.index_for_point(point(current.x, px(target_y)));
        self.move_to(ix, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.end, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.previous_word_boundary(self.cursor_offset());
        self.move_to(prev, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_word_boundary(self.cursor_offset());
        self.move_to(next, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // Multiline input: newlines are welcome (unlike the single-line example).
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ComposerInputEvent::Submitted);
    }

    // ---- geometry ----

    /// Content-local point for a byte index (y grows down from content top).
    fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let line_start = *self.line_starts.get(line_ix)?;
            let line_len = line.len();
            if index < line_start {
                continue;
            }
            if index <= line_start + line_len {
                let local = line.position_for_index(index - line_start, self.line_height)?;
                let y_offset: f32 = self
                    .last_lines
                    .iter()
                    .take(line_ix)
                    .map(|l| f32::from(l.size(self.line_height).height))
                    .sum();
                return Some(point(local.x, local.y + px(y_offset)));
            }
        }
        None
    }

    /// Byte index closest to a content-local point.
    fn index_for_point(&self, position: Point<Pixels>) -> usize {
        if self.display_is_placeholder {
            return 0;
        }
        let mut y = f32::from(position.y);
        if y < 0.0 {
            return 0;
        }
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let height = f32::from(line.size(self.line_height).height);
            let line_start = self.line_starts.get(line_ix).copied().unwrap_or(0);
            if y < height || line_ix + 1 == self.last_lines.len() {
                let local = point(position.x, px(y.min(height - 1.0).max(0.0)));
                let ix = line
                    .closest_index_for_position(local, self.line_height)
                    .unwrap_or_else(|ix| ix);
                return (line_start + ix).min(self.content.len());
            }
            y -= height;
        }
        self.content.len()
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let local = point(
            position.x - bounds.left(),
            position.y - bounds.top() + px(self.scroll_top),
        );
        self.index_for_point(local)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let index = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    // ---- utf16 mapping (IME) ----

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Shape the text at a width; store measured layout; return content height.
    /// Called from the element's measured-layout closure.
    fn layout_text(&mut self, width: Pixels, style: &TextStyle, window: &mut Window) -> f32 {
        let (display, is_placeholder) = if self.content.is_empty() {
            (self.placeholder.clone(), true)
        } else {
            (SharedString::from(self.content.clone()), false)
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        self.line_height = px(INPUT_LINE_HEIGHT);

        let run_for = |len: usize, underline: bool| TextRun {
            len,
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: underline.then_some(UnderlineStyle {
                color: Some(style.color),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: None,
        };
        let runs: Vec<TextRun> = match self.marked_range.as_ref() {
            Some(marked) if !is_placeholder => vec![
                run_for(marked.start, false),
                run_for(marked.len(), true),
                run_for(display.len() - marked.end, false),
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            _ => vec![run_for(display.len(), false)],
        };

        let lines = window
            .text_system()
            .shape_text(display, font_size, &runs, Some(width), None)
            .map(|small| small.into_vec())
            .unwrap_or_default();

        // Logical line byte offsets (each shaped line covers one \n-split line).
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        for line in &lines {
            line_starts.push(at);
            at += line.len() + 1; // + '\n'
        }
        if line_starts.is_empty() {
            line_starts.push(0);
        }

        let content_height: f32 = lines
            .iter()
            .map(|l| f32::from(l.size(self.line_height).height))
            .sum();
        let max_line_width: f32 = lines
            .iter()
            .map(|l| f32::from(l.unwrapped_layout.width))
            .fold(0.0, f32::max);

        self.display_is_placeholder = is_placeholder;
        self.last_lines = lines;
        self.line_starts = line_starts;
        self.content_height = content_height.max(INPUT_LINE_HEIGHT);
        self.max_line_width = if is_placeholder { 0.0 } else { max_line_width };
        self.last_width = f32::from(width);
        self.content_height
    }

    /// Keep the cursor visible when content exceeds the element height.
    fn clamp_scroll(&mut self, element_height: f32) {
        let max_scroll = (self.content_height - element_height).max(0.0);
        if let Some(cursor) = self.point_for_index(self.cursor_offset()) {
            let cursor_top = f32::from(cursor.y);
            let cursor_bottom = cursor_top + f32::from(self.line_height);
            if cursor_top < self.scroll_top {
                self.scroll_top = cursor_top;
            } else if cursor_bottom > self.scroll_top + element_height {
                self.scroll_top = cursor_bottom - element_height;
            }
        }
        self.scroll_top = self.scroll_top.clamp(0.0, max_scroll);
    }
}

impl EventEmitter<ComposerInputEvent> for ComposerInput {}

impl Focusable for ComposerInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for ComposerInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.point_for_index(range.start)?;
        let origin = point(
            bounds.left() + start.x,
            bounds.top() + start.y - px(self.scroll_top),
        );
        Some(Bounds::new(origin, size(px(2.0), self.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point_in_window: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for_mouse_position(point_in_window);
        Some(self.offset_to_utf16(index))
    }
}

/// The custom element: measured auto-grow layout + shaped-line painting.
struct ComposerTextElement {
    input: Entity<ComposerInput>,
    /// Max content height before internal scrolling kicks in.
    max_content_height: f32,
}

struct ComposerTextPrepaint {
    cursor: Option<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
}

impl IntoElement for ComposerTextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for ComposerTextElement {
    type RequestLayoutState = ();
    type PrepaintState = ComposerTextPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        let input = self.input.clone();
        let text_style = window.text_style();
        let max_content = self.max_content_height;
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.unwrap_or(match available.width {
                    gpui::AvailableSpace::Definite(width) => width,
                    _ => px(320.0),
                });
                let content_height =
                    input.update(cx, |input, _| input.layout_text(width, &text_style, window));
                size(width, px(content_height.min(max_content)))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.input.update(cx, |input, _| {
            input.clamp_scroll(f32::from(bounds.size.height));
            input.last_bounds = Some(bounds);
        });
        let input = self.input.read(cx);
        let scroll = px(input.scroll_top);
        let origin = point(bounds.left(), bounds.top() - scroll);
        let selection_color = gpui::hsla(0.66, 0.6, 0.55, 0.35);

        let mut selection_quads = Vec::new();
        let mut cursor = None;
        if input.selected_range.is_empty() || input.display_is_placeholder {
            if let Some(p) = input.point_for_index(input.cursor_offset()) {
                cursor = Some(fill(
                    Bounds::new(
                        point(origin.x + p.x, origin.y + p.y),
                        size(px(2.0), input.line_height),
                    ),
                    gpui::hsla(0.66, 0.7, 0.7, 1.0),
                ));
            } else if input.display_is_placeholder {
                cursor = Some(fill(
                    Bounds::new(origin, size(px(2.0), input.line_height)),
                    gpui::hsla(0.66, 0.7, 0.7, 1.0),
                ));
            }
        } else if let (Some(start), Some(end)) = (
            input.point_for_index(input.selected_range.start),
            input.point_for_index(input.selected_range.end),
        ) {
            let lh = input.line_height;
            if start.y == end.y {
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(origin.x + end.x, origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
            } else {
                // First visual row, full middle rows, last visual row.
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(bounds.right(), origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
                if end.y > start.y + lh {
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            point(origin.x, origin.y + start.y + lh),
                            point(bounds.right(), origin.y + end.y),
                        ),
                        selection_color,
                    ));
                }
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x, origin.y + end.y),
                        point(origin.x + end.x, origin.y + end.y + lh),
                    ),
                    selection_color,
                ));
            }
        }
        ComposerTextPrepaint {
            cursor,
            selection_quads,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // WrappedLine isn't Clone — temporarily take the shaped lines out of the
        // entity for painting, then put them back for mouse mapping.
        let (lines, line_height, scroll) = self.input.update(cx, |input, _| {
            (
                std::mem::take(&mut input.last_lines),
                input.line_height,
                input.scroll_top,
            )
        });

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.selection_quads.drain(..) {
                window.paint_quad(quad);
            }
            let mut y = bounds.top() - px(scroll);
            for line in &lines {
                let height = line.size(line_height).height;
                let _ = line.paint(
                    point(bounds.left(), y),
                    line_height,
                    gpui::TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                );
                y += height;
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_lines = lines;
        });
    }
}

impl Render for ComposerInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let text_color = if self.content.is_empty() {
            theme.text_faint
        } else {
            theme.text
        };
        div()
            .key_context("Composer")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .text_size(px(INPUT_TEXT_SIZE))
            .line_height(px(INPUT_LINE_HEIGHT))
            .text_color(text_color)
            .font_family(theme.font_sans.clone())
            .child(ComposerTextElement {
                input: cx.entity(),
                max_content_height: COMPOSER_MAX_HEIGHT - INPUT_VERTICAL_CHROME,
            })
    }
}

// ---------------------------------------------------------------------------
// Composer wrapper
// ---------------------------------------------------------------------------

/// Events the shell listens for.
#[derive(Debug, Clone)]
pub enum ComposerEvent {
    /// A prompt was sent (optimistically) — re-engage the transcript pin.
    Sent { chat_id: String },
}

pub struct Composer {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// Composer actions row: repo/branch/harness-model/traits (§1.7).
    pickers: Entity<Pickers>,
    /// Draft text per chat key ("" = new-chat canvas), surviving navigation.
    drafts: HashMap<String, String>,
    current_key: String,
    sending: bool,
    failure: Option<SharedString>,
    wizard: Option<Wizard>,
    wizard_focus: FocusHandle,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    answered_requests: HashSet<String>,
    advance_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    _observe: Subscription,
    _input_events: Subscription,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::new("Do anything…", cx));
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.on_submit(cx),
            ComposerInputEvent::Edited => cx.notify(),
        });
        let current_key = state.read(cx).selected_chat.clone().unwrap_or_default();
        Self {
            state,
            input,
            pickers,
            drafts: HashMap::new(),
            current_key,
            sending: false,
            failure: None,
            wizard: None,
            wizard_focus: cx.focus_handle(),
            answered_requests: HashSet::new(),
            advance_task: None,
            send_task: None,
            _observe: observe,
            _input_events: input_events,
        }
    }

    pub fn is_sending(&self) -> bool {
        self.sending
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (key, pending) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone().unwrap_or_default(),
                pending_input_request(&s.transcript),
            )
        };

        // Draft swap on chat navigation — the input entity itself survives.
        if key != self.current_key {
            let old_text = self.input.read(cx).text().to_string();
            if old_text.is_empty() {
                self.drafts.remove(&self.current_key);
            } else {
                self.drafts.insert(self.current_key.clone(), old_text);
            }
            let draft = self.drafts.get(&key).cloned().unwrap_or_default();
            self.current_key = key;
            self.failure = None;
            self.wizard = None;
            self.input.update(cx, |input, cx| input.set_text(draft, cx));
        }

        // Question panel lifecycle (wizard state cached per request id).
        match pending {
            Some((request_id, questions)) if !self.answered_requests.contains(&request_id) => {
                let same = self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id);
                if !same {
                    self.wizard = Some(Wizard::new(request_id, questions));
                    self.advance_task = None;
                    // The shared input becomes the panel's free-text override.
                    self.input.update(cx, |input, cx| {
                        input.set_placeholder(
                            "Type your own answer, or pick an option above",
                            cx,
                        )
                    });
                }
            }
            _ => {
                if self.wizard.is_some() {
                    self.wizard = None;
                    self.advance_task = None;
                    self.input
                        .update(cx, |input, cx| input.set_placeholder("Do anything…", cx));
                }
            }
        }
        cx.notify();
    }

    fn run_live(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        let Some(chat_id) = s.selected_chat.as_deref() else {
            return false;
        };
        matches!(
            s.indicator_for(chat_id, chrono::Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        )
    }

    fn button_mode(&self, cx: &App) -> SendButtonMode {
        let has_text = !self.input.read(cx).text().trim().is_empty();
        send_button_mode(self.run_live(cx), has_text)
    }

    fn on_submit(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            // Enter inside the panel's free-text input submits the page.
            let typed = self.input.read(cx).text().trim().to_string();
            if let Some(w) = self.wizard.as_mut() {
                w.set_typed(typed);
            }
            self.wizard_advance(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        match self.button_mode(cx) {
            SendButtonMode::Stop => self.interrupt(cx),
            _ if text.is_empty() => {}
            SendButtonMode::Send => self.send(text, false, cx),
            SendButtonMode::Steer => self.send(text, true, cx),
        }
    }

    /// Queue a Run (or Steer) doc command with an optimistic echo. New chats
    /// thread the picked config in: worktree creation (when the isolated toggle
    /// is on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
    /// reasoning / options on the Run request itself (§1.7).
    fn send(&mut self, text: String, steer: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        // Chat id: existing selection, or client-minted for the new-chat canvas
        // (the chat then appears from the doc host once the doc materializes).
        let (chat_id, is_new) = match self.state.read(cx).selected_chat.clone() {
            Some(id) => (id, false),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        let draft = self.pickers.read(cx).draft().clone();
        let existing_cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.cwd.clone());
        let device_id = {
            let state = self.state.read(cx);
            state
                .local_device_id
                .clone()
                .or_else(|| state.devices.first().map(|d| d.id.clone()))
                .unwrap_or_else(|| "local".to_string())
        };
        let message_id = uuid::Uuid::new_v4().to_string();

        // Optimistic echo (client-minted id doubles as the persisted message id,
        // so the doc frame dedups it away).
        let echo = SessionMessageEntry {
            id: message_id.clone(),
            role: comet_doc::MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.clone(),
            }],
            created_at: chrono::Utc::now().timestamp_millis(),
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        self.state.update(cx, |s, cx| {
            if is_new {
                s.select_chat(Some(chat_id.clone()), cx);
            }
            s.push_echo(&chat_id, echo);
            cx.notify();
        });

        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&self.current_key);
        self.failure = None;
        self.sending = true;
        cx.emit(ComposerEvent::Sent {
            chat_id: chat_id.clone(),
        });
        cx.notify();

        let steer_cmd = steer && !is_new;
        let restore_text = text.clone();
        let err_chat_id = chat_id.clone();
        let err_message_id = message_id.clone();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<(), String> = async {
                // Resolve the working directory: existing chats keep theirs;
                // new chats use the picked repo — via a fresh isolated worktree
                // when the toggle is on (CreateWorktree on send).
                let mut cwd = if is_new {
                    draft.repo.as_ref().map(|r| r.path.clone())
                } else {
                    existing_cwd
                }
                .unwrap_or_else(|| ".".to_string());
                if is_new
                    && draft.isolated_worktree
                    && let (Some(repo), Some(branch)) = (&draft.repo, &draft.branch)
                {
                    let params = serde_json::json!({
                        "repoPath": repo.path,
                        "branch": branch,
                    });
                    let value = engine
                        .client()
                        .call(methods::CREATE_WORKTREE, params)
                        .await
                        .map_err(|e| format!("Worktree failed: {e}"))?;
                    let worktree: comet_proto::Worktree = serde_json::from_value(value)
                        .map_err(|e| format!("Worktree reply malformed: {e}"))?;
                    cwd = worktree.path;
                }

                // Best-effort Mutate createChat with the picked config
                // (idempotent engine-side; the doc host would materialize the
                // chat on first command anyway, so failures are non-fatal).
                if is_new {
                    let mut mutate = serde_json::json!({
                        "op": "createChat",
                        "chatId": chat_id,
                        "deviceId": device_id,
                        "cwd": cwd,
                    });
                    if let (Some(config), Some(object)) =
                        (draft.chat_config(), mutate.as_object_mut())
                        && let Ok(config) = serde_json::to_value(&config)
                    {
                        object.insert("config".into(), config);
                    }
                    if let Err(err) = engine.client().call(methods::MUTATE, mutate).await {
                        tracing::debug!(error = %err, "CreateChat mutate unavailable; doc host will materialize the chat");
                    }
                }

                let command = if steer_cmd {
                    SessionCommandPayload::Steer {
                        prompt: text.clone(),
                        message_id: Some(message_id.clone()),
                    }
                } else {
                    SessionCommandPayload::Run {
                        request: RunRequest {
                            prompt: text.clone(),
                            model: draft.model.clone(),
                            reasoning: draft.reasoning,
                            model_options: draft.model_options.clone(),
                            cwd,
                            sandbox: SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            resume: None,
                        },
                        message_id: message_id.clone(),
                    }
                };
                let command = serde_json::to_value(&command)
                    .map_err(|e| format!("Send failed: {e}"))?;
                let params = serde_json::json!({ "chatId": chat_id, "command": command });
                engine
                    .client()
                    .call(methods::QUEUE_COMMAND, params)
                    .await
                    .map_err(|e| format!("Send failed: {e}"))?;
                Ok(())
            }
            .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                if let Err(message) = result {
                    // Failure: red banner, echo removed, prompt back in the draft.
                    composer.failure = Some(message.into());
                    composer.state.update(cx, |s, cx| {
                        s.remove_echo(&err_chat_id, &err_message_id);
                        cx.notify();
                    });
                    composer.input.update(cx, |input, cx| input.set_text(restore_text, cx));
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let params = serde_json::json!({
            "chatId": chat_id,
            "command": { "kind": "interrupt" },
        });
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Stop failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    // ---- wizard glue ----

    fn wizard_select(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let step = wizard.select(option_ix);
        let has_pick = wizard.page_has_pick();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                if has_pick {
                    "Type your own answer, or leave this blank to use the selected option"
                } else {
                    "Type your own answer, or pick an option above"
                },
                cx,
            )
        });
        match step {
            WizardStep::AutoAdvance => self.schedule_auto_advance(cx),
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            WizardStep::Stay => {}
        }
        cx.notify();
    }

    fn schedule_auto_advance(&mut self, cx: &mut Context<Self>) {
        self.advance_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTO_ADVANCE_MS))
                .await;
            this.update(cx, |composer, cx| composer.wizard_advance(cx))
                .ok();
        }));
    }

    fn wizard_advance(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        match wizard.advance() {
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            _ => {
                // Moving on: clear the shared free-text input for the next page.
                self.input.update(cx, |input, cx| input.set_text("", cx));
                cx.notify();
            }
        }
    }

    fn wizard_back(&mut self, cx: &mut Context<Self>) {
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.back();
            cx.notify();
        }
    }

    /// Submit RespondInput and retire the panel.
    fn wizard_finish(&mut self, answers: Vec<UserInputAnswer>, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        self.advance_task = None;
        self.answered_requests.insert(wizard.request_id.clone());
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            // The panel borrowed the composer input; hand back its identity.
            input.set_placeholder("Do anything…", cx);
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let command = SessionCommandPayload::RespondInput {
            request_id: wizard.request_id.clone(),
            answers,
        };
        let params = match serde_json::to_value(&command) {
            Ok(value) => serde_json::json!({ "chatId": chat_id, "command": value }),
            Err(_) => return,
        };
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Answer failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn on_wizard_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // Keys bubbling out of the free-text input must not double-handle:
        // digits select options only while the input is empty, and Enter is the
        // input's own Submit action when it has focus.
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let input_empty = self.input.read(cx).is_empty();
        let key = event.keystroke.key.as_str();
        if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
        {
            if !input_focused || input_empty {
                self.wizard_select(digit - 1, cx);
                // Consumed as a selection: stop the platform from also
                // inserting the digit into the focused free-text input.
                cx.stop_propagation();
            }
        } else if key == "enter" {
            if !input_focused {
                self.wizard_advance(cx);
                cx.stop_propagation();
            }
        } else if key == "escape" && (!input_focused || input_empty) {
            self.wizard_back(cx);
            cx.stop_propagation();
        }
    }

    // ---- render pieces ----

    /// The agent-asked-a-question panel (comet question-panel.tsx), rendered in
    /// place of the composer: the same floating-pill chrome (`rounded-[26px]
    /// border-white/[0.08] bg-white/[0.03] shadow-xl`), uppercase header +
    /// "1/3" counter chip, option rows with number kbd chips, a free-text
    /// override over a hairline, and Back / Next-Submit footer.
    fn render_wizard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(wizard) = self.wizard.clone() else {
            return gpui::Empty.into_any_element();
        };
        let counter = wizard.counter();
        let Some(question) = wizard.current().cloned() else {
            return gpui::Empty.into_any_element();
        };
        let page = wizard.page;
        let last = page + 1 >= wizard.questions.len();
        let typed_empty = self.input.read(cx).is_empty();
        let can_advance = wizard.page_has_pick() || !typed_empty;

        let options = question.options.iter().enumerate().map(|(ix, label)| {
            // Selection reads on the row only while no typed override exists
            // (typed answers win — comet question-panel.tsx `isSel`).
            let picked = wizard.is_picked(ix) && typed_empty;
            div()
                .id(("wizard-option", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(if picked {
                    crate::theme::white_alpha(0.16)
                } else {
                    gpui::transparent_black()
                })
                .bg(if picked {
                    crate::theme::white_alpha(0.09)
                } else {
                    crate::theme::white_alpha(0.025)
                })
                .when(!picked, |el| {
                    el.hover(|s| s.bg(crate::theme::white_alpha(0.06)))
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.wizard_select(ix, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if picked {
                            theme.text
                        } else {
                            theme.text.opacity(0.9)
                        })
                        .child(SharedString::from(label.clone())),
                )
                .when(ix < 9, |el| {
                    el.child(
                        // Number kbd chip: `size-[22px] rounded-md text-[11px]`.
                        div()
                            .flex_none()
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .bg(if picked {
                                crate::theme::white_alpha(0.16)
                            } else {
                                crate::theme::white_alpha(0.05)
                            })
                            .text_size(px(11.0))
                            .text_color(if picked {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.6)
                            })
                            .child(SharedString::from(format!("{}", ix + 1))),
                    )
                })
        });

        div()
            .id("question-panel")
            .track_focus(&self.wizard_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_wizard_key(event, window, cx)
            }))
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(crate::theme::white_alpha(0.03))
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.0))
                    .pt(px(16.0))
                    .flex()
                    .flex_col()
                    // Header: tracked uppercase + counter chip when paged.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(crate::popover::tracked_upper(
                                        &question.header,
                                    ))),
                            )
                            .when(wizard.questions.len() > 1, |el| {
                                el.child(
                                    div()
                                        .h(px(20.0))
                                        .px(px(6.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(6.0))
                                        .bg(crate::theme::white_alpha(0.06))
                                        .text_size(px(10.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text_muted.opacity(0.6))
                                        .child(SharedString::from(counter)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .text_size(px(15.0))
                            .line_height(px(20.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(question.question.clone())),
                    )
                    .when(question.multi_select, |el| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted.opacity(0.65))
                                .child(SharedString::from("Select one or more options.")),
                        )
                    })
                    .child(div().mt(px(12.0)).flex().flex_col().gap(px(4.0)).children(options))
                    // Free-text override over a hairline (shares the composer
                    // input entity).
                    .child(
                        div()
                            .mt(px(12.0))
                            .border_t_1()
                            .border_color(crate::theme::white_alpha(0.06))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .px(px(4.0))
                            .child(self.input.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .pt(px(4.0))
                    .child(if page > 0 {
                        crate::popover::btn_ghost(&theme, "Back")
                            .id("wizard-back")
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_back(cx)))
                            .into_any_element()
                    } else {
                        gpui::Empty.into_any_element()
                    })
                    .child(
                        crate::popover::btn_primary(
                            &theme,
                            if last { "Submit" } else { "Next" },
                        )
                        .id("wizard-submit")
                        .px(px(16.0))
                        .when(!can_advance, |el| el.opacity(0.4))
                        .on_click(cx.listener(|this, _, _, cx| this.wizard_advance(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_send_button(
        &mut self,
        mode: SendButtonMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::of(cx);
        // Comet composer-actions.tsx: a size-7 filled circle — up-arrow to
        // send/steer, a dark rounded square on the same light circle to stop.
        match mode {
            SendButtonMode::Stop => div()
                .id("composer-stop")
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .child(div().size(px(11.0)).rounded(px(3.0)).bg(theme.bg))
                .into_any_element(),
            SendButtonMode::Send | SendButtonMode::Steer => div()
                .id("composer-send")
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.on_submit(cx)))
                .child(
                    crate::icons::icon(crate::icons::ARROW_UP)
                        .size(px(14.0))
                        .text_color(theme.bg),
                )
                .into_any_element(),
        }
    }
}

/// Focus lands on the prompt input (window-level focus fallbacks — e.g. after
/// the focused terminal panel is hidden — route here).
impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let wizard_active = self.wizard.is_some();
        let mode = self.button_mode(cx);
        let (text_width, has_newline, content_height, last_width) = {
            let input = self.input.read(cx);
            (
                input.measured_text_width(),
                input.has_newline(),
                input.measured_content_height(),
                input.last_width,
            )
        };
        // Pill capacity ≈ the input's own last measured width (compact renders
        // constrain it); before first measure default to compact.
        let capacity = if last_width > 0.0 {
            last_width - 8.0
        } else {
            f32::MAX
        };
        let expanded = composer_expanded(text_width, capacity, has_newline);
        let total_height = composer_total_height(content_height);

        let failure = self.failure.clone();
        // Centered composer column (comet `mx-auto w-full max-w-3xl`).
        let container = div()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            .when_some(failure, |el, message| {
                el.child(
                    div()
                        .id("composer-failure")
                        .px(px(10.0))
                        .py(px(6.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.failure = None;
                            cx.notify();
                        }))
                        .child(message),
                )
            });

        if wizard_active {
            let wizard = self.render_wizard(cx);
            return container.child(motion::fade_quick("composer-wizard", div().child(wizard)));
        }

        // New chats always use the expanded layout: the repo/branch pickers
        // need the full-width actions row (comet composer-actions.tsx).
        let new_chat = self.state.read(cx).selected_chat.is_none();
        let expanded = expanded || new_chat;

        let send_button = self.render_send_button(mode, cx);
        // Attach button (visual affordance; uploads arrive via paste/drop).
        let attach = div()
            .id("composer-attach")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::white_alpha(0.10)))
            .child(
                crate::icons::icon(crate::icons::PAPERCLIP)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );

        // The pill chrome (comet composer.tsx): `rounded-[26px] border
        // border-white/[0.08] bg-white/[0.03] shadow-xl` — a floating pill with
        // a hairline over a faint wash, never a solid grey box. Picker chips,
        // attach, and the send circle all live INSIDE the pill.
        let pill_bg = crate::theme::white_alpha(0.03);
        let pill = div()
            .rounded(px(26.0))
            .bg(pill_bg)
            .border_1()
            .border_color(theme.border)
            .shadow_lg();
        let body = if expanded {
            // Expanded: textarea on top (`px-4 pb-1 pt-4`), actions row below
            // (`px-3 pb-2.5 pt-1`), auto-grow between the height bounds.
            pill.h(px(total_height))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .px(px(16.0))
                        .pt(px(16.0))
                        .pb(px(4.0))
                        .child(self.input.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .pl(px(12.0))
                        .pr(px(10.0))
                        .pt(px(4.0))
                        .pb(px(10.0))
                        .child(div().flex_1().min_w_0().child(self.pickers.clone()))
                        .child(attach)
                        .child(send_button),
                )
        } else {
            // Compact pill: input and the actions cluster on one line
            // (`py-3 pl-4 pr-2` textarea, `gap-2 py-1.5 pl-1 pr-2` cluster).
            pill.h(px(46.0))
                .flex()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pl(px(16.0))
                        .pr(px(8.0))
                        .child(self.input.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(4.0))
                        .pr(px(8.0))
                        .child(div().flex_none().child(self.pickers.clone()))
                        .child(attach)
                        .child(send_button),
                )
        };
        container.child(motion::fade_quick("composer-input", body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str, options: &[&str], multi: bool) -> UserInputQuestion {
        UserInputQuestion {
            id: id.into(),
            header: "Header".into(),
            question: format!("Question {id}"),
            options: options.iter().map(|s| s.to_string()).collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn flip_decision() {
        // Fits in the pill → compact.
        assert!(!composer_expanded(150.0, 300.0, false));
        // Overflow → expanded.
        assert!(composer_expanded(320.0, 300.0, false));
        // Newline always expands.
        assert!(composer_expanded(10.0, 300.0, true));
        // Narrow column (< MIN_COMPACT_INPUT_WIDTH) always expands.
        assert!(composer_expanded(10.0, 199.0, false));
        assert!(!composer_expanded(10.0, 200.0, false));
    }

    #[test]
    fn auto_grow_math() {
        // One line sits at the floor (line + chrome).
        assert_eq!(
            composer_total_height(input_content_height(1)),
            COMPOSER_MIN_HEIGHT
        );
        // Growth is linear once content exceeds the floor.
        let h4 = composer_total_height(input_content_height(4));
        assert_eq!(h4, 4.0 * INPUT_LINE_HEIGHT + INPUT_VERTICAL_CHROME);
        // Caps at 260px of textarea content plus chrome (comet max-h-[260px]).
        assert_eq!(
            composer_total_height(input_content_height(100)),
            COMPOSER_MAX_HEIGHT
        );
        // Zero lines still measures one.
        assert_eq!(input_content_height(0), INPUT_LINE_HEIGHT);
    }

    #[test]
    fn send_button_morph() {
        assert_eq!(send_button_mode(false, false), SendButtonMode::Send);
        assert_eq!(send_button_mode(false, true), SendButtonMode::Send);
        assert_eq!(send_button_mode(true, true), SendButtonMode::Steer);
        assert_eq!(send_button_mode(true, false), SendButtonMode::Stop);
    }

    #[test]
    fn wizard_single_select_auto_advances_and_completes() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a", "b"], false),
                question("q2", &["x"], false),
            ],
        );
        assert_eq!(w.counter(), "1/2");
        assert_eq!(w.select(1), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.advance(), WizardStep::Stay);
        assert_eq!(w.counter(), "2/2");
        assert_eq!(w.select(0), WizardStep::AutoAdvance);
        let WizardStep::Done(answers) = w.advance() else {
            panic!("expected Done")
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].labels, vec!["b"]);
        assert_eq!(answers[1].labels, vec!["x"]);
    }

    #[test]
    fn wizard_multi_select_toggles_and_stays() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b", "c"], true)]);
        assert_eq!(w.select(0), WizardStep::Stay);
        assert_eq!(w.select(2), WizardStep::Stay);
        assert!(w.is_picked(0) && w.is_picked(2));
        // Toggle off.
        assert_eq!(w.select(0), WizardStep::Stay);
        assert!(!w.is_picked(0));
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["c"]);
    }

    #[test]
    fn wizard_number_keys_and_bounds() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b"], false)]);
        assert_eq!(w.press_number(9), WizardStep::Stay, "out of range ignored");
        assert_eq!(w.press_number(0), WizardStep::Stay);
        assert_eq!(w.press_number(2), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.select(5), WizardStep::Stay, "bad option ix ignored");
    }

    #[test]
    fn wizard_typed_answer_overrides_and_back_pages() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a"], false),
                question("q2", &["x", "y"], false),
            ],
        );
        w.select(0);
        w.advance();
        assert_eq!(w.page, 1);
        assert!(w.back());
        assert_eq!(w.page, 0);
        assert!(!w.back(), "already at first page");
        w.advance();
        w.set_typed("  custom answer  ".into());
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["a"]);
        assert_eq!(
            answers[1].labels,
            vec!["custom answer"],
            "typed overrides picked, trimmed"
        );
    }

    #[test]
    fn pending_input_detection() {
        use comet_doc::MessageRole;
        let input_part = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![question("q", &["a"], false)],
            resolved: false,
        };
        let entry = |status: Option<MessageStatus>, parts: Vec<MessagePart>| SessionMessageEntry {
            id: "m".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
        };
        // Streaming entry with unresolved input → panel.
        let t = vec![entry(
            Some(MessageStatus::Streaming),
            vec![input_part.clone()],
        )];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // Completed entry → no panel even if unresolved (run is gone).
        let t = vec![entry(
            Some(MessageStatus::Complete),
            vec![input_part.clone()],
        )];
        assert!(pending_input_request(&t).is_none());
        // Resolved part → no panel.
        let resolved = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![],
            resolved: true,
        };
        let t = vec![entry(Some(MessageStatus::Streaming), vec![resolved])];
        assert!(pending_input_request(&t).is_none());
        assert!(pending_input_request(&[]).is_none());
    }
}
