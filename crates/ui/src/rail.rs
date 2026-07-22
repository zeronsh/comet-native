//! MessageRail (feature-inventory §1.8): a left vertical minimap of the user's
//! prompts. The active tick brightens, hover grows the tick and shows a preview
//! card (prompt + reply opening), click smooth-scrolls the transcript to that
//! row. Hidden below a 48rem container width.
//!
//! Pure logic (tick extraction, active detection, width gate, previews) lives
//! in free functions with unit tests; rendering is an `impl Transcript`
//! extension since the rail shares the transcript's rows and `ListState`.

use gpui::{AnyElement, Context, ListOffset, SharedString, div, prelude::*, px};
use std::time::{Duration, Instant};

use comet_doc::{MessagePart, MessageRole, SessionMessageEntry};

use crate::motion;
use crate::popover;
use crate::theme::Theme;
use crate::transcript::Transcript;

/// 48rem — the container width below which the rail (and wide gutters) collapse.
pub const RAIL_MIN_CONTAINER_WIDTH: f32 = 768.0;

pub fn rail_visible(container_width: f32) -> bool {
    container_width >= RAIL_MIN_CONTAINER_WIDTH
}

/// Preview text caps (grapheme-unaware char cut is fine for a preview card).
pub const PREVIEW_PROMPT_CHARS: usize = 160;
pub const PREVIEW_REPLY_CHARS: usize = 200;

/// One rail tick: a user prompt and the opening of the reply that followed.
#[derive(Debug, Clone, PartialEq)]
pub struct RailTick {
    /// Message id — equals the user row's id in the transcript row model.
    pub message_id: String,
    pub prompt: String,
    pub reply: Option<String>,
}

fn user_text(entry: &SessionMessageEntry) -> String {
    let raw = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    // Attachment refs ride the message text — the rail shows the visible
    // prompt, or "Attached image(s)" for image-only sends
    // (message-attachments.ts `userMessageRailText`).
    crate::attachments::user_message_rail_text(&raw)
}

fn first_reply_text(entries: &[SessionMessageEntry]) -> Option<String> {
    entries
        .iter()
        .find(|e| e.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                    Some(text.trim().to_string())
                }
                _ => None,
            })
        })
}

/// Extract rail ticks from the transcript: one per user entry (doc entries
/// first, then unconfirmed echoes — matching transcript row order). Each tick
/// carries the opening of the assistant reply that followed it, for the hover
/// preview card.
pub fn rail_ticks(
    entries: &[SessionMessageEntry],
    echoes: &[SessionMessageEntry],
) -> Vec<RailTick> {
    let mut ticks: Vec<RailTick> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        if entry.role != MessageRole::User {
            continue;
        }
        ticks.push(RailTick {
            message_id: entry.id.clone(),
            prompt: user_text(entry),
            reply: first_reply_text(&entries[ix + 1..]),
        });
    }
    for echo in echoes {
        if echo.role == MessageRole::User && !ticks.iter().any(|t| t.message_id == echo.id) {
            ticks.push(RailTick {
                message_id: echo.id.clone(),
                prompt: user_text(echo),
                reply: None,
            });
        }
    }
    ticks
}

/// The active tick for a scroll position: the last tick whose transcript row is
/// at or above the viewport-top row (the prompt whose section you're reading).
/// Before the first tick's row, the first tick is active.
pub fn active_tick(tick_rows: &[usize], top_row: usize) -> Option<usize> {
    if tick_rows.is_empty() {
        return None;
    }
    match tick_rows.iter().rposition(|&row| row <= top_row) {
        Some(ix) => Some(ix),
        None => Some(0),
    }
}

// ---------------------------------------------------------------------------
// Fixed-footprint outline (shadcn MessageScroller "Transcript Outline")
// ---------------------------------------------------------------------------

/// One tick's hit-row height and the gap between ticks (message-rail.tsx).
pub const TICK_SLOT: f32 = 10.0;
pub const TICK_GAP: f32 = 3.0;
/// Vertical breathing room kept clear above/below the tick stack.
pub const RAIL_V_MARGIN: f32 = 24.0;

/// Hard cap on visible ticks — the always-compact shadcn outline (its demo
/// trigger holds a small fixed stack of micro-marks regardless of window
/// height), not "fill the viewport, then condense".
pub const MAX_RAIL_TICKS: usize = 12;

/// How many tick slots fit in a rail of `height` px (always ≥ 1).
pub fn rail_capacity(height: f32) -> usize {
    let usable = (height - 2.0 * RAIL_V_MARGIN).max(TICK_SLOT);
    (((usable + TICK_GAP) / (TICK_SLOT + TICK_GAP)).floor() as usize).max(1)
}

/// Slots the rail actually uses: what fits the viewport, hard-capped at
/// [`MAX_RAIL_TICKS`] so the outline stays compact even on tall windows.
pub fn rail_slots(height: f32) -> usize {
    rail_capacity(height).min(MAX_RAIL_TICKS)
}

/// shadcn's Transcript Outline keeps the always-visible rail a FIXED footprint
/// (tiny marks in a fixed-size trigger; the full turn list lives behind it) —
/// it never grows with the conversation. Same idea here: when prompts
/// outnumber the slots that fit the viewport, ticks become evenly-sized
/// BUCKETS over the conversation (a downsampled minimap) instead of
/// overflowing. Returns each bucket's `[start, end)` tick range; with
/// `n <= capacity` every bucket is a single tick — the identity, i.e. the
/// old per-prompt rail.
pub fn tick_buckets(n: usize, capacity: usize) -> Vec<(usize, usize)> {
    if n == 0 {
        return Vec::new();
    }
    let cap = capacity.clamp(1, n);
    (0..cap).map(|k| (k * n / cap, (k + 1) * n / cap)).collect()
}

/// The bucket containing tick `ix` (for active/hover mapping).
pub fn bucket_of(buckets: &[(usize, usize)], ix: usize) -> Option<usize> {
    buckets.iter().position(|&(s, e)| ix >= s && ix < e)
}

/// Char-cap a preview with an ellipsis. Whitespace runs (including newlines —
/// prompts and replies are free text) collapse to single spaces first: the
/// preview card's title is a one-line surface (message-rail.tsx line-clamp-1).
pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    let flat = crate::transcript::single_line(text);
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

// ---------------------------------------------------------------------------
// Duration-based glide timeline (pure)
// ---------------------------------------------------------------------------

/// Duration-based scroll glide (browser smooth-scroll parity: the Electron
/// rail used `scrollToItem({behavior:"smooth"})` — a fixed-duration gentle
/// ease over the WHOLE distance, never percent-of-remaining).
///
/// Rows above the viewport are unmeasured, so the total pixel distance can
/// only be ESTIMATED per frame. The timeline therefore hands out each frame's
/// movement as a fraction of whatever distance currently remains:
/// `(e_now − e_prev) / (1 − e_prev)` for eased progress `e`. With a stable
/// estimate this telescopes to exactly `start + e(t)·total` — the fixed eased
/// timeline. When the estimate changes mid-flight (a row got measured, the
/// bottom-aligned layout re-glued an anchor), the SAME timeline simply
/// continues over the corrected remainder — no restart, no compensating jump.
#[derive(Debug, Clone)]
pub struct GlideTimeline {
    eased_prev: f32,
}

impl Default for GlideTimeline {
    fn default() -> Self {
        Self::new()
    }
}

impl GlideTimeline {
    pub fn new() -> Self {
        Self { eased_prev: 0.0 }
    }

    /// Fraction of the CURRENT remaining distance to consume for eased
    /// progress `eased` (monotone, `0..=1`; `1.0` lands exactly).
    pub fn step(&mut self, eased: f32) -> f32 {
        let eased = eased.clamp(self.eased_prev, 1.0);
        let denom = 1.0 - self.eased_prev;
        let frac = if denom <= 1e-6 {
            1.0
        } else {
            (eased - self.eased_prev) / denom
        };
        self.eased_prev = eased;
        frac.clamp(0.0, 1.0)
    }
}

/// `COMET_SCROLL_TRACE=1` logs per-frame glide positions at `warn` level —
/// the smoothness measurement knob (same family as `COMET_FRAME_STATS`).
fn scroll_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("COMET_SCROLL_TRACE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

// ---------------------------------------------------------------------------
// Rendering + smooth scroll (Transcript extension)
// ---------------------------------------------------------------------------

impl Transcript {
    /// Smooth-scroll the list so `target` sits at the viewport top, reusing the
    /// transcript scroll-task slot (any running stick/jump animation yields).
    ///
    /// A [`motion::SCROLL_GLIDE`] (500ms ease-in-out) timeline drives every
    /// frame's position; per-frame movement comes from the timeline, never
    /// from a percent of the remaining distance:
    ///
    /// - a glued bottom anchor (`item_ix == len`, one viewport BELOW the
    ///   visible top) is first materialized as the true viewport-top anchor —
    ///   stepping straight from the glued anchor lands inside the re-glue band
    ///   and layout undoes it every frame (the old stall→double-jump path);
    /// - rows above the viewport are unmeasured, so the anchor glides in item
    ///   space along the same timeline, estimating sub-row offsets from a
    ///   local row-height EMA; the position is read back each frame, so a
    ///   measurement correcting the estimate just re-enters the timeline;
    /// - once the target row is measured the glide is pixel-exact.
    pub fn scroll_to_row(&mut self, target: usize, cx: &mut Context<Self>) {
        if motion::reduced_motion(cx) {
            self.list_state().scroll_to(ListOffset {
                item_ix: target,
                offset_in_item: px(0.0),
            });
            cx.notify();
            return;
        }
        self.set_scroll_task(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let total = motion::SCROLL_GLIDE.total().mul_f32(motion::speed_scale());
            let mut timeline = GlideTimeline::new();
            let mut height_ema: Option<f32> = None;
            let trace = scroll_trace_enabled();
            let frames = (total.as_millis() / 16) as usize + 90;
            for _ in 0..frames {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let raw = (started.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0);
                let eased = motion::SCROLL_GLIDE.curve.eval(raw);
                let frac = timeline.step(eased);
                let done = this.update(cx, |t, cx| {
                    let list = t.list_state().clone();
                    if raw >= 1.0 {
                        list.scroll_to(ListOffset {
                            item_ix: target,
                            offset_in_item: px(0.0),
                        });
                        cx.notify();
                        return true;
                    }
                    // Materialize the glued bottom representation as the true
                    // top anchor (same visual position, sticky anchor).
                    let viewport = f32::from(list.viewport_bounds().size.height);
                    if t.is_glued() && viewport > 0.0 {
                        list.scroll_by(px(-(viewport + 0.5)));
                    }
                    let top = list.logical_scroll_top();
                    let top_height = list
                        .bounds_for_item(top.item_ix)
                        .map(|b| f32::from(b.size.height).max(1.0));
                    // Row-height estimate for unmeasured territory: the mean
                    // over the whole visible span, recomputed per frame (the
                    // ~dozen mixed row kinds in a viewport average out — a
                    // single-row estimate whipsaws between paragraphs and
                    // code blocks and modulates the per-frame step visibly).
                    if viewport > 0.0 {
                        let bottom = f32::from(list.viewport_bounds().bottom());
                        let mut ix = top.item_ix;
                        let mut count = 0.0f32;
                        while let Some(b) = list.bounds_for_item(ix) {
                            if f32::from(b.top()) >= bottom {
                                break;
                            }
                            count += 1.0;
                            ix += 1;
                        }
                        if count > 0.0 {
                            let mean = viewport / count;
                            let ema = height_ema.get_or_insert(mean);
                            *ema += 0.5 * (mean - *ema);
                        }
                    }
                    if height_ema.is_none() {
                        height_ema = top_height;
                    }
                    // Where the viewport top actually is, in fractional item
                    // space — read back per frame (self-correcting: an anchor
                    // the layout adjusted or re-glued keeps its real remaining
                    // distance and continues the same timeline).
                    let here = top.item_ix as f32
                        + top_height
                            .map(|h| (f32::from(top.offset_in_item) / h).clamp(0.0, 1.0))
                            .unwrap_or(0.0);
                    if trace {
                        tracing::warn!(
                            ms = started.elapsed().as_millis() as u64,
                            eased,
                            here,
                            dist = t.distance_from_bottom(),
                            "scroll-glide"
                        );
                    }

                    if target < top.item_ix {
                        // Above the viewport (unmeasured): progressive
                        // item-space anchoring within the eased timeline.
                        let next = here - frac * (here - target as f32);
                        // Small steps ride `scroll_by` — the list keeps a
                        // 320px measured leading overdraw, so a step that
                        // fits inside it crosses rows at their TRUE heights
                        // (pixel-exact frames through the gentle start and
                        // landing, where jitter would show most).
                        let step_px = (here - next) * height_ema.unwrap_or(0.0);
                        if step_px > 0.0 && step_px <= crate::transcript::OVERDRAW_PX * 0.8 {
                            list.scroll_by(px(-step_px));
                            cx.notify();
                            return false;
                        }
                        let ix = (next.floor().max(0.0) as usize).min(top.item_ix);
                        let within = next - ix as f32;
                        let offset = if ix == top.item_ix {
                            // Same row as the current anchor: measured height,
                            // pixel-exact — and never below the current offset,
                            // so motion stays monotone even when a height
                            // estimate was corrected.
                            top_height
                                .map(|h| (within * h).min(f32::from(top.offset_in_item)))
                                .unwrap_or(0.0)
                        } else {
                            within * height_ema.unwrap_or(0.0)
                        };
                        list.scroll_to(ListOffset {
                            item_ix: ix,
                            offset_in_item: px(offset),
                        });
                        cx.notify();
                        return false;
                    }
                    match list.bounds_for_item(target) {
                        Some(bounds) => {
                            // Measured: pixel-exact step along the timeline.
                            let delta = f32::from(bounds.top() - list.viewport_bounds().top());
                            list.scroll_by(px(frac * delta));
                        }
                        None => {
                            // Below but unmeasured: item space, same timeline.
                            let next = here + frac * (target as f32 - here);
                            let ix = (next.floor().max(0.0) as usize).min(target);
                            let within = next - ix as f32;
                            list.scroll_to(ListOffset {
                                item_ix: ix,
                                offset_in_item: px(within * height_ema.unwrap_or(0.0)),
                            });
                        }
                    }
                    cx.notify();
                    false
                });
                match done {
                    Ok(true) | Err(_) => return,
                    Ok(false) => {}
                }
            }
            // Timeline exhausted (shouldn't happen): land exactly.
            this.update(cx, |t, cx| {
                t.list_state().scroll_to(ListOffset {
                    item_ix: target,
                    offset_in_item: px(0.0),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// The rail element — an absolute overlay along the transcript's left edge.
    pub fn render_rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.rail_enabled() {
            return gpui::Empty.into_any_element();
        }
        let (entries, echoes) = {
            let state = self.state_entity().read(cx);
            (state.transcript.clone(), state.pending_echoes().to_vec())
        };
        let ticks = rail_ticks(&entries, &echoes);
        // Map each tick to its transcript row (user rows share the entry id).
        let pairs: Vec<(RailTick, usize)> = ticks
            .into_iter()
            .filter_map(|tick| {
                let row = self
                    .rows()
                    .iter()
                    .position(|r| r.id.as_ref() == tick.message_id.as_str())?;
                Some((tick, row))
            })
            .collect();
        // A minimap of one exchange is noise, not navigation — the original
        // rail hides below two marks (message-rail.tsx `marks.length < 2`).
        if pairs.len() < 2 {
            return gpui::Empty.into_any_element();
        }
        let tick_rows: Vec<usize> = pairs.iter().map(|(_, row)| *row).collect();
        let top_row = self.list_state().logical_scroll_top().item_ix;
        let active = active_tick(&tick_rows, top_row);
        let hover = self.rail_hover();
        let theme = Theme::of(cx).clone();

        // Fixed footprint (shadcn Transcript Outline): a compact stack of at
        // most MAX_RAIL_TICKS marks — past that, ticks become even buckets
        // over the conversation. Pre-layout the viewport reads 0; assume a
        // typical height for that one frame rather than collapsing to a
        // single tick.
        let viewport_h = f32::from(self.list_state().viewport_bounds().size.height);
        let capacity = rail_slots(if viewport_h > 0.0 { viewport_h } else { 600.0 });
        let buckets = tick_buckets(pairs.len(), capacity);
        let active_bucket = active.and_then(|ix| bucket_of(&buckets, ix));

        div()
            .absolute()
            .left(px(16.0))
            .top_0()
            .bottom_0()
            .w(px(26.0))
            .flex()
            .flex_col()
            .items_start()
            .justify_center()
            .gap(px(TICK_GAP))
            .children(buckets.into_iter().enumerate().map(|(ix, (start, end))| {
                // The bucket's representative prompt: the ACTIVE tick when it
                // falls inside (hover then previews what you're reading),
                // the first prompt of the range otherwise.
                let rep = active.filter(|&a| a >= start && a < end).unwrap_or(start);
                let (tick, row) = &pairs[rep];
                let (tick, row) = (tick.clone(), *row);
                let bucket_len = end - start;
                let is_active = active_bucket == Some(ix);
                let is_hovered = hover == Some(ix);
                // Only hover grows the tick; the active one just reads brighter
                // (message-rail.tsx: w-3 rest, w-5 hovered).
                let bar_width = if is_hovered { 20.0 } else { 12.0 };
                let bar_color = if is_active || is_hovered {
                    theme.text.opacity(0.8)
                } else {
                    crate::theme::white_alpha(0.16)
                };
                let prompt = truncate_preview(&tick.prompt, PREVIEW_PROMPT_CHARS);
                let reply = tick
                    .reply
                    .as_deref()
                    .map(|r| truncate_preview(r, PREVIEW_REPLY_CHARS));
                let card: Option<AnyElement> = is_hovered.then(|| {
                    popover::popover_card(&theme)
                        .w(px(280.0))
                        .p(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(SharedString::from(prompt.clone())),
                        )
                        .when_some(reply.clone(), |el, reply| {
                            el.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(reply)),
                            )
                        })
                        // Condensed bucket: say how many prompts it stands for
                        // (the outline still spans the whole conversation).
                        .when(bucket_len > 1, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(format!("{bucket_len} prompts"))),
                            )
                        })
                        .into_any_element()
                });
                div()
                    .id(("rail-tick", ix))
                    .relative()
                    .h(px(TICK_SLOT))
                    .w_full()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        this.set_rail_hover(if *hovered { Some(ix) } else { None });
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_row(row, cx);
                    }))
                    .child(
                        div()
                            .h(px(2.0))
                            .w(px(bar_width))
                            .rounded(px(1.0))
                            .bg(bar_color),
                    )
                    .when_some(card, |el, card| {
                        el.child(gpui::deferred(
                            gpui::anchored()
                                .anchor(gpui::Anchor::LeftCenter)
                                .snap_to_window_with_margin(px(8.0))
                                .child(div().pl(px(26.0)).child(card)),
                        ))
                    })
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_doc::MessageStatus;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn capacity_counts_slots_that_fit() {
        // 880px viewport − 48 margin = 832 usable → (832+3)/13 = 64 slots.
        assert_eq!(rail_capacity(880.0), 64);
        // Tiny (or unmeasured) heights still hand out one slot.
        assert_eq!(rail_capacity(0.0), 1);
        assert!(rail_capacity(200.0) >= 10);
        // The rail itself is hard-capped: compact on any window height.
        assert_eq!(rail_slots(880.0), MAX_RAIL_TICKS);
        assert_eq!(rail_slots(2000.0), MAX_RAIL_TICKS);
        // Short rails still shrink below the cap.
        assert!(rail_slots(100.0) < MAX_RAIL_TICKS);
    }

    #[test]
    fn buckets_are_identity_under_capacity() {
        // n <= capacity: one tick per prompt — the old per-prompt rail.
        let b = tick_buckets(5, 64);
        assert_eq!(b.len(), 5);
        assert!(
            b.iter()
                .enumerate()
                .all(|(k, &(s, e))| s == k && e == k + 1)
        );
    }

    #[test]
    fn buckets_partition_evenly_over_capacity() {
        // 100 prompts into 8 slots: every tick in exactly one bucket, in
        // order, first starts at 0, last ends at n, sizes within ±1 of even.
        let n = 100;
        let b = tick_buckets(n, 8);
        assert_eq!(b.len(), 8);
        assert_eq!(b[0].0, 0);
        assert_eq!(b.last().unwrap().1, n);
        for w in b.windows(2) {
            assert_eq!(w[0].1, w[1].0, "contiguous");
        }
        for &(s, e) in &b {
            assert!((e - s) == 12 || (e - s) == 13, "even split, got {}", e - s);
        }
    }

    #[test]
    fn bucket_of_maps_ticks_to_their_bucket() {
        let b = tick_buckets(10, 3); // [0,3) [3,6) [6,10)
        assert_eq!(bucket_of(&b, 0), Some(0));
        assert_eq!(bucket_of(&b, 3), Some(1));
        assert_eq!(bucket_of(&b, 9), Some(2));
        assert_eq!(bucket_of(&b, 10), None);
        // Degenerate inputs.
        assert!(tick_buckets(0, 8).is_empty());
        assert_eq!(tick_buckets(3, 0), vec![(0, 3)]);
    }

    #[test]
    fn ticks_map_user_prompts_with_reply_openings() {
        let entries = vec![
            entry("u1", MessageRole::User, "first question"),
            entry("a1", MessageRole::Assistant, "first answer"),
            entry("u2", MessageRole::User, "second question"),
            entry("a2", MessageRole::Assistant, "second answer"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].message_id, "u1");
        assert_eq!(ticks[0].prompt, "first question");
        assert_eq!(ticks[0].reply.as_deref(), Some("first answer"));
        assert_eq!(ticks[1].reply.as_deref(), Some("second answer"));
    }

    #[test]
    fn ticks_include_echoes_deduped() {
        let entries = vec![entry("u1", MessageRole::User, "sent")];
        let echoes = vec![
            entry("u1", MessageRole::User, "sent"), // confirmed already → deduped
            entry("u2", MessageRole::User, "pending"),
        ];
        let ticks = rail_ticks(&entries, &echoes);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[1].message_id, "u2");
        assert_eq!(ticks[1].reply, None);
    }

    #[test]
    fn tick_without_reply_yet() {
        let entries = vec![
            entry("u1", MessageRole::User, "q"),
            entry("a1", MessageRole::Assistant, "reply to first"),
            entry("u2", MessageRole::User, "latest"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        // The last prompt has no assistant entry after it.
        assert_eq!(ticks[1].reply, None);
        // Empty transcript → no ticks.
        assert!(rail_ticks(&[], &[]).is_empty());
    }

    #[test]
    fn active_tick_tracks_viewport_top() {
        let tick_rows = [0, 5, 9];
        assert_eq!(active_tick(&tick_rows, 0), Some(0));
        assert_eq!(active_tick(&tick_rows, 4), Some(0));
        assert_eq!(active_tick(&tick_rows, 5), Some(1));
        assert_eq!(active_tick(&tick_rows, 8), Some(1));
        assert_eq!(active_tick(&tick_rows, 100), Some(2));
        // Above the first tick row → first tick still active.
        assert_eq!(active_tick(&[3, 7], 1), Some(0));
        assert_eq!(active_tick(&[], 4), None);
    }

    #[test]
    fn rail_width_gate() {
        assert!(rail_visible(768.0));
        assert!(rail_visible(1200.0));
        assert!(!rail_visible(767.9));
        assert!(!rail_visible(0.0));
    }

    /// Consuming `(e'−e)/(1−e)` of the current remainder telescopes to exactly
    /// the absolute eased timeline `start + e(t)·total` when the distance
    /// estimate is stable — the glide is timeline-driven, not
    /// percent-of-remaining.
    #[test]
    fn glide_timeline_matches_absolute_eased_interpolation() {
        let curve = motion::SCROLL_GLIDE.curve;
        let mut timeline = GlideTimeline::new();
        let (start, target) = (1000.0f32, 0.0f32);
        let mut pos = start;
        for i in 1..=60 {
            let t = i as f32 / 60.0;
            let eased = curve.eval(t);
            let frac = timeline.step(eased);
            pos -= frac * (pos - target);
            let absolute = start + eased * (target - start);
            assert!(
                (pos - absolute).abs() < 0.05,
                "frame {i}: pos {pos} != absolute {absolute}"
            );
        }
        assert_eq!(pos, target); // eased hits 1.0 → frac 1.0 → exact landing.
    }

    /// A mid-flight distance re-estimate (anchor re-glued / row measured)
    /// continues the SAME timeline over the corrected remainder: no restart,
    /// no compensating jump, exact landing.
    #[test]
    fn glide_timeline_survives_remaining_distance_reestimate() {
        let curve = motion::SCROLL_GLIDE.curve;
        let mut timeline = GlideTimeline::new();
        let mut pos = 500.0f32;
        let mut prev_frac = 0.0f32;
        for i in 1..=60 {
            let t = i as f32 / 60.0;
            let frac = timeline.step(curve.eval(t));
            if i == 30 {
                // The layout re-glued the anchor: remaining distance doubles.
                pos *= 2.0;
            }
            pos -= frac * pos;
            // Fractions depend only on the timeline — the re-estimate cannot
            // make a step consume a larger share than the curve dictates.
            assert!((0.0..=1.0).contains(&frac));
            if i > 1 && i < 55 {
                assert!(frac >= prev_frac - 0.05, "frame {i}: frac regressed");
            }
            prev_frac = frac;
        }
        assert_eq!(pos, 0.0);
    }

    /// Timeline steps clamp: regressions in eased input yield zero movement,
    /// and completion always yields the full remainder.
    #[test]
    fn glide_timeline_step_clamps() {
        let mut timeline = GlideTimeline::new();
        assert_eq!(timeline.step(0.4), 0.4);
        assert_eq!(timeline.step(0.3), 0.0); // non-monotone input → no move
        assert_eq!(timeline.step(1.0), 1.0); // done → land exactly
        assert_eq!(timeline.step(1.0), 1.0); // idempotent at the end
    }

    /// The first 16ms frame of the 500ms glide covers under 2% of the
    /// distance — no first-frame majority jump by construction.
    #[test]
    fn glide_first_frame_is_gentle() {
        let spec = motion::SCROLL_GLIDE;
        assert_eq!(spec.duration_ms, 500);
        let first = spec.curve.eval(16.0 / 500.0);
        assert!(first < 0.02, "first frame covered {first} of the distance");
        // And the ease-in-out midpoint is exactly half the distance.
        let mid = spec.curve.eval(0.5);
        assert!((mid - 0.5).abs() < 0.01);
    }

    #[test]
    fn preview_truncation() {
        assert_eq!(truncate_preview("short", 10), "short");
        assert_eq!(truncate_preview("  padded  ", 10), "padded");
        let long = "x".repeat(50);
        let cut = truncate_preview(&long, 10);
        assert!(cut.chars().count() <= 10);
        assert!(cut.ends_with('…'));
        // Multi-byte safety.
        let uni = "héllo wörld attaché case overflowing";
        let cut = truncate_preview(uni, 12);
        assert!(cut.ends_with('…'));
    }
}
