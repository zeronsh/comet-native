//! Streaming fade veil — per-appended-chunk opacity over already-committed text.
//!
//! The desktop app (docs/research/mugen-pretext.md §2e) commits streamed text to
//! layout instantly and dissolves a purely cosmetic veil over the newly arrived
//! characters. This module is the gpui port of that idea:
//!
//! - [`ElemVeil`] tracks, per rendered text element, the previously rendered
//!   flat text. Each append registers the new byte range as a [`Chunk`] with its
//!   arrival time; a fast stream keeps several chunks fading concurrently (a
//!   rolling veil). A chunk fades exactly once — already-faded text never
//!   re-animates, and a fully settled element returns no spans at all.
//! - [`apply_veil`] multiplies the fading alpha into the `TextRun` colors
//!   covering each chunk. This is paint-only by construction: a color-only run
//!   split cannot change layout — gpui shapes text through cosmic-text, whose
//!   `Attrs::compatible` ignores color/metadata, so adjacent same-font runs are
//!   shaped as one contiguous word even across the split (kerning and ligatures
//!   survive; wrapping is byte-identical to the unsplit render).
//!
//! The fade is [`crate::motion::FADE_IN`]'s 500ms expo-out curve with **zero
//! translate** — opacity only, per the "no positional offset on streamed
//! content" rule.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use gpui::TextRun;

use crate::motion::{FADE_IN, MotionSpec};

/// The veil's fade spec — the entrance curve, opacity only.
pub const VEIL_FADE: MotionSpec = FADE_IN;

/// One appended chunk of text mid-fade.
#[derive(Debug, Clone)]
struct Chunk {
    /// Byte range in the element's flat text.
    range: Range<usize>,
    started: Instant,
}

/// A veiled byte range and its current opacity (0..1).
pub type VeilSpan = (Range<usize>, f32);

/// Veil opacity for a chunk age. Pure — unit-testable.
pub fn veil_opacity(elapsed_ms: f32) -> f32 {
    let total = VEIL_FADE.total().as_millis() as f32;
    if total <= 0.0 {
        return 1.0;
    }
    VEIL_FADE.progress((elapsed_ms / total).clamp(0.0, 1.0))
}

/// Per-element chunk tracker: remembers the last rendered flat text and fades
/// every newly appended suffix exactly once.
#[derive(Debug, Default)]
pub struct ElemVeil {
    prev: String,
    chunks: Vec<Chunk>,
}

/// Longest common prefix length, snapped back to a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let mut p = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while p > 0 && !b.is_char_boundary(p) {
        p -= 1;
    }
    p
}

impl ElemVeil {
    /// Advance to `text` at `now`: registers a fading chunk for newly appended
    /// bytes, prunes settled chunks, and returns the active spans. Idempotent
    /// for unchanged text — safe to call once per frame (or twice, when a row
    /// is both measured and painted).
    pub fn advance(&mut self, text: &str, now: Instant) -> Vec<VeilSpan> {
        if text != self.prev {
            // Non-append rewrites (the incremental parser re-deriving a block's
            // flat text — e.g. `**bold` collapsing into a bold run) keep the
            // common prefix's committed fades and re-veil only the changed tail.
            let p = common_prefix(&self.prev, text);
            self.chunks.retain_mut(|c| {
                c.range.end = c.range.end.min(p);
                c.range.start < c.range.end
            });
            if text.len() > p {
                self.chunks.push(Chunk {
                    range: p..text.len(),
                    started: now,
                });
            }
            self.prev.clear();
            self.prev.push_str(text);
        }
        let total = VEIL_FADE.total();
        self.chunks
            .retain(|c| now.saturating_duration_since(c.started) < total);
        self.chunks
            .iter()
            .map(|c| {
                let elapsed = now.saturating_duration_since(c.started);
                (c.range.clone(), veil_opacity(elapsed.as_millis() as f32))
            })
            .collect()
    }

    /// Any chunk still fading (as of the last [`advance`](Self::advance))?
    pub fn is_fading(&self) -> bool {
        !self.chunks.is_empty()
    }
}

/// Veil state for one live streaming row, keyed by the render tree's stable
/// per-element discriminator (top-level block ix / nested ix scheme — stable
/// across appends because the incremental parser only reparses the tail).
#[derive(Debug, Default)]
pub struct RowVeil {
    elems: HashMap<usize, ElemVeil>,
}

impl RowVeil {
    pub fn advance(&mut self, elem: usize, text: &str, now: Instant) -> Vec<VeilSpan> {
        self.elems.entry(elem).or_default().advance(text, now)
    }

    /// Any element still fading? Drives the once-per-frame repaint request.
    pub fn is_fading(&self) -> bool {
        self.elems.values().any(ElemVeil::is_fading)
    }
}

/// Intersect spans with `[start, end)` and shift them to local offsets — used
/// by per-line code rendering where chunks are tracked on the whole code text.
pub fn slice_spans(spans: &[VeilSpan], start: usize, end: usize) -> Vec<VeilSpan> {
    spans
        .iter()
        .filter_map(|(r, a)| {
            let s = r.start.max(start);
            let e = r.end.min(end);
            (s < e).then(|| (s - start..e - start, *a))
        })
        .collect()
}

/// Multiply veil opacities into the runs' paint colors, splitting runs at span
/// boundaries. Fonts, lengths, and text are untouched — the total run length is
/// preserved exactly, so shaping and wrapping cannot change (see module docs).
pub fn apply_veil(runs: Vec<TextRun>, spans: &[VeilSpan]) -> Vec<TextRun> {
    if spans.is_empty() || spans.iter().all(|(_, a)| *a >= 1.0) {
        return runs;
    }
    let mut out = Vec::with_capacity(runs.len() + spans.len() * 2);
    let mut pos = 0usize;
    for run in runs {
        let (start, end) = (pos, pos + run.len);
        pos = end;
        let mut cuts = vec![start, end];
        for (r, _) in spans {
            if r.start > start && r.start < end {
                cuts.push(r.start);
            }
            if r.end > start && r.end < end {
                cuts.push(r.end);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (s, e) = (w[0], w[1]);
            let mut piece = run.clone();
            piece.len = e - s;
            if let Some(alpha) = spans
                .iter()
                .find(|(r, _)| r.start <= s && e <= r.end)
                .map(|(_, a)| *a)
                && alpha < 1.0
            {
                piece.color = piece.color.opacity(alpha);
                piece.background_color = piece.background_color.map(|c| c.opacity(alpha));
                if let Some(u) = &mut piece.underline {
                    u.color = u.color.map(|c| c.opacity(alpha));
                }
                if let Some(st) = &mut piece.strikethrough {
                    st.color = st.color.map(|c| c.opacity(alpha));
                }
            }
            out.push(piece);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{font, px};
    use std::time::Duration;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_text_fades_and_settles_once() {
        let t0 = Instant::now();
        let mut v = ElemVeil::default();
        let spans = v.advance("hello", t0);
        assert_eq!(spans, vec![(0..5, 0.0)]);
        // Mid-fade: opacity strictly between 0 and 1, range unchanged.
        let spans = v.advance("hello", at(t0, 250));
        assert_eq!(spans.len(), 1);
        assert!(spans[0].1 > 0.0 && spans[0].1 < 1.0);
        // Settled: pruned, never re-animates.
        assert!(v.advance("hello", at(t0, 600)).is_empty());
        assert!(!v.is_fading());
        assert!(v.advance("hello", at(t0, 700)).is_empty());
    }

    #[test]
    fn appended_chunks_fade_concurrently_and_independently() {
        let t0 = Instant::now();
        let mut v = ElemVeil::default();
        v.advance("one ", t0);
        let spans = v.advance("one two ", at(t0, 100));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, 0..4);
        assert_eq!(spans[1].0, 4..8);
        // The older chunk is further along its fade than the newer one.
        assert!(spans[0].1 > spans[1].1);
        // After the first settles, only the newer chunk remains.
        let spans = v.advance("one two three", at(t0, 550));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, 4..8);
        assert_eq!(spans[1].0, 8..13);
    }

    #[test]
    fn faded_text_never_reanimates_on_append() {
        let t0 = Instant::now();
        let mut v = ElemVeil::default();
        v.advance("stable", t0);
        // Fully settled...
        assert!(v.advance("stable", at(t0, 600)).is_empty());
        // ...then an append veils ONLY the new suffix.
        let spans = v.advance("stable more", at(t0, 700));
        assert_eq!(spans, vec![(6..11, 0.0)]);
    }

    #[test]
    fn non_append_rewrite_keeps_prefix_and_reveils_tail() {
        let t0 = Instant::now();
        let mut v = ElemVeil::default();
        v.advance("intro **bol", t0);
        // Markdown resolves: flat text loses the `**` marker.
        let spans = v.advance("intro bold", at(t0, 100));
        // The shared prefix's chunk is clamped, the changed tail is one chunk.
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, 0..6);
        assert_eq!(spans[1].0, 6..10);
        assert!(spans[0].1 > spans[1].1);
    }

    #[test]
    fn common_prefix_respects_char_boundaries() {
        // "é" = 0xC3 0xA9, "è" = 0xC3 0xA8 — byte prefix would split the char.
        assert_eq!(common_prefix("é", "è"), 0);
        assert_eq!(common_prefix("abé", "abè"), 2);
        assert_eq!(common_prefix("same", "same"), 4);
    }

    #[test]
    fn veil_opacity_curve_endpoints() {
        assert_eq!(veil_opacity(0.0), 0.0);
        assert_eq!(veil_opacity(500.0), 1.0);
        let mid = veil_opacity(250.0);
        assert!(mid > 0.0 && mid < 1.0);
        // Monotonic.
        assert!(veil_opacity(100.0) <= veil_opacity(200.0));
    }

    fn run(len: usize, color: gpui::Hsla) -> TextRun {
        TextRun {
            len,
            font: font("Test"),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    #[test]
    fn apply_veil_preserves_length_and_fonts() {
        let color = gpui::white();
        let runs = vec![run(4, color), run(6, color)];
        let spans = vec![(2..8, 0.5)];
        let out = apply_veil(runs.clone(), &spans);
        let total: usize = out.iter().map(|r| r.len).sum();
        assert_eq!(total, 10, "split must cover the text exactly");
        assert!(out.iter().all(|r| r.font == runs[0].font));
        // Pieces: [0..2 full] [2..4 faded] [4..8 faded] [8..10 full].
        assert_eq!(
            out.iter().map(|r| r.len).collect::<Vec<_>>(),
            vec![2, 2, 4, 2]
        );
        assert_eq!(out[0].color.a, 1.0);
        assert_eq!(out[1].color.a, 0.5);
        assert_eq!(out[2].color.a, 0.5);
        assert_eq!(out[3].color.a, 1.0);
    }

    #[test]
    fn apply_veil_without_spans_is_identity() {
        let runs = vec![run(5, gpui::white())];
        let out = apply_veil(runs.clone(), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len, 5);
        // Settled spans (alpha 1) also pass straight through unsplit — the
        // settled frame is byte-identical to an unsplit render.
        let out = apply_veil(runs.clone(), &[(0..5, 1.0)]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn apply_veil_fades_decorations_too() {
        let mut r = run(5, gpui::white());
        r.background_color = Some(gpui::white());
        r.underline = Some(gpui::UnderlineStyle {
            color: Some(gpui::white()),
            thickness: px(1.0),
            wavy: false,
        });
        let out = apply_veil(vec![r], &[(0..5, 0.25)]);
        assert_eq!(out[0].background_color.unwrap().a, 0.25);
        assert_eq!(out[0].underline.as_ref().unwrap().color.unwrap().a, 0.25);
    }

    #[test]
    fn slice_spans_shifts_to_local_offsets() {
        let spans = vec![(3..10, 0.4), (12..20, 0.1)];
        // A "line" covering bytes 5..15.
        let local = slice_spans(&spans, 5, 15);
        assert_eq!(local, vec![(0..5, 0.4), (7..10, 0.1)]);
        // Disjoint window → empty.
        assert!(slice_spans(&spans, 25, 30).is_empty());
    }

    #[test]
    fn row_veil_tracks_elements_independently() {
        let t0 = Instant::now();
        let mut row = RowVeil::default();
        row.advance(0, "para", t0);
        row.advance(2, "code", t0);
        assert!(row.is_fading());
        row.advance(0, "para", at(t0, 600));
        assert!(row.is_fading(), "elem 2 hasn't been advanced past its fade");
        row.advance(2, "code", at(t0, 600));
        assert!(!row.is_fading());
    }
}
