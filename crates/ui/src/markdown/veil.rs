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
//! Constants and curve match mugen-markdown's web `FadePainter` (the engine the
//! desktop app ships): per-chunk duration adapts to the stream's cadence — an
//! EMA of inter-append gaps, `duration = clamp(ema × 3, 120ms, 400ms)` — the
//! dissolve eases with `veil = (1 − p)^1.6` (text alpha `1 − veil`), and a
//! backed-up stream (3+ concurrent chunks) gets a small speed boost. **Zero
//! translate** — opacity only, per the "no positional offset on streamed
//! content" rule.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use gpui::TextRun;

/// EMA seed for the inter-append gap (mugen `EMA_SEED_MS`).
pub const VEIL_EMA_SEED_MS: f32 = 160.0;
/// Duration clamp (mugen `MIN_FADE_MS` / `MAX_FADE_MS`).
pub const VEIL_MIN_FADE_MS: f32 = 120.0;
pub const VEIL_MAX_FADE_MS: f32 = 400.0;
/// Dissolve exponent (mugen: `alpha = (1 - p) ** 1.6`).
pub const VEIL_CURVE_POW: f32 = 1.6;
/// Gap clamp feeding the EMA (mugen: `min(gap, 1000)`).
const VEIL_GAP_CLAMP_MS: f32 = 1000.0;

/// One appended chunk of text mid-fade.
#[derive(Debug, Clone)]
struct Chunk {
    /// Byte range in the element's flat text.
    range: Range<usize>,
    started: Instant,
    /// Fade duration fixed at arrival from the cadence EMA.
    duration_ms: f32,
}

/// A veiled byte range and its current opacity (0..1).
pub type VeilSpan = (Range<usize>, f32);

/// Text alpha for a fade progress `p` (0..1): the veil dissolves as
/// `(1 − p)^1.6`, so the text shows through at `1 − veil`. Pure.
pub fn veil_opacity(p: f32) -> f32 {
    1.0 - (1.0 - p.clamp(0.0, 1.0)).powf(VEIL_CURVE_POW)
}

/// Chunk fade duration for the current inter-append EMA (mugen:
/// `min(MAX, max(MIN, ema * 3))`).
pub fn veil_duration_ms(ema_ms: f32) -> f32 {
    (ema_ms * 3.0).clamp(VEIL_MIN_FADE_MS, VEIL_MAX_FADE_MS)
}

/// Fast-stream boost: 3+ chunks fading concurrently speed up by 30% each
/// (mugen: `1 + 0.3 * max(0, veils.length - 2)`).
pub fn veil_boost(active_chunks: usize) -> f32 {
    1.0 + 0.3 * active_chunks.saturating_sub(2) as f32
}

/// EMA update on a new append gap (mugen: `ema*0.7 + min(gap,1000)*0.3`).
pub fn veil_ema_next(ema_ms: f32, gap_ms: f32) -> f32 {
    ema_ms * 0.7 + gap_ms.min(VEIL_GAP_CLAMP_MS) * 0.3
}

/// Per-element chunk tracker: remembers the last rendered flat text and fades
/// every newly appended suffix exactly once.
#[derive(Debug)]
pub struct ElemVeil {
    prev: String,
    chunks: Vec<Chunk>,
    /// EMA of inter-append gaps (drives per-chunk durations).
    ema_ms: f32,
    last_append: Option<Instant>,
}

impl Default for ElemVeil {
    fn default() -> Self {
        Self {
            prev: String::new(),
            chunks: Vec::new(),
            ema_ms: VEIL_EMA_SEED_MS,
            last_append: None,
        }
    }
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
                // Cadence-adaptive duration (mugen FadePainter): update the
                // EMA with the gap since the previous append.
                if let Some(last) = self.last_append {
                    let gap = now.saturating_duration_since(last).as_secs_f32() * 1000.0;
                    self.ema_ms = veil_ema_next(self.ema_ms, gap);
                }
                self.last_append = Some(now);
                self.chunks.push(Chunk {
                    range: p..text.len(),
                    started: now,
                    duration_ms: veil_duration_ms(self.ema_ms),
                });
            }
            self.prev.clear();
            self.prev.push_str(text);
        }
        let boost = veil_boost(self.chunks.len());
        self.chunks.retain(|c| {
            let elapsed = now.saturating_duration_since(c.started).as_secs_f32() * 1000.0;
            elapsed * boost < c.duration_ms
        });
        let boost = veil_boost(self.chunks.len());
        self.chunks
            .iter()
            .map(|c| {
                let elapsed = now.saturating_duration_since(c.started).as_secs_f32() * 1000.0;
                let progress = (elapsed * boost / c.duration_ms).clamp(0.0, 1.0);
                (c.range.clone(), veil_opacity(progress))
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
        // After the first settles (past its adaptive duration), only the
        // newer chunk remains.
        let spans = v.advance("one two ", at(t0, 410));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 4..8);
    }

    #[test]
    fn fade_constants_match_mugen_fade_painter() {
        // @wingleeio/mugen-markdown dist/index.mjs: EMA_SEED_MS=160,
        // MIN_FADE_MS=120, MAX_FADE_MS=400, alpha=(1-p)**1.6,
        // ema = ema*0.7 + min(gap,1000)*0.3, boost = 1 + 0.3*max(0,n-2).
        assert_eq!(VEIL_EMA_SEED_MS, 160.0);
        assert_eq!(VEIL_MIN_FADE_MS, 120.0);
        assert_eq!(VEIL_MAX_FADE_MS, 400.0);
        assert_eq!(VEIL_CURVE_POW, 1.6);
        // duration = clamp(ema*3, 120, 400).
        assert_eq!(veil_duration_ms(160.0), 400.0); // seed → clamped at max
        assert_eq!(veil_duration_ms(30.0), 120.0); // fast stream → floor
        assert_eq!(veil_duration_ms(60.0), 180.0);
        // EMA update.
        assert_eq!(veil_ema_next(160.0, 100.0), 160.0 * 0.7 + 100.0 * 0.3);
        assert_eq!(veil_ema_next(160.0, 5000.0), 160.0 * 0.7 + 1000.0 * 0.3);
        // Fast-stream boost kicks in at the 3rd concurrent chunk.
        assert_eq!(veil_boost(0), 1.0);
        assert_eq!(veil_boost(2), 1.0);
        assert!((veil_boost(3) - 1.3).abs() < 1e-6);
        assert!((veil_boost(5) - 1.9).abs() < 1e-6);
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
        // Text alpha = 1 - (1-p)^1.6: 0 at arrival, 1 when the veil is gone.
        assert_eq!(veil_opacity(0.0), 0.0);
        assert_eq!(veil_opacity(1.0), 1.0);
        let mid = veil_opacity(0.5);
        assert!(mid > 0.0 && mid < 1.0);
        // The pow-1.6 ease-out reveals faster than linear early on.
        assert!(mid > 0.5);
        assert!((mid - (1.0 - 0.5f32.powf(1.6))).abs() < 1e-6);
        // Monotonic + clamped.
        assert!(veil_opacity(0.2) <= veil_opacity(0.4));
        assert_eq!(veil_opacity(-1.0), 0.0);
        assert_eq!(veil_opacity(2.0), 1.0);
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
