//! Loaders: the comet pulse loader, the gradient matrix spinner, and the boot
//! splash content. All motion routes through `crate::motion` pure helpers, so
//! the math is unit-tested and these elements are testable-by-compile.
//!
//! Rendering pattern: each cell is its own `with_animation` repeating element
//! sharing one period; per-cell offsets come from [`motion::staggered_phase`],
//! so all cells stay phase-locked (they start on the same frame) without a
//! shared clock. Cells animate inside fixed-size slots — opacity and inner size
//! are paint-local and never move surrounding layout. Reduced motion snaps every
//! cell to its rest state automatically (gpui `reduce_motion`).

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};

use crate::motion::{
    self, AnimationExt as _, COMET_PULSE, GRADIENT_SPIN, PULSE_STAGGER, SPLASH_OUT,
};
use crate::theme::Theme;

/// Cells in the comet wave loader.
pub const COMET_CELLS: usize = 5;
/// Side length of the gradient spinner matrix.
pub const MATRIX_SIDE: usize = 3;

/// The comet mark's pixels — `[x, y]` of each 100×100 cell on the 820×940
/// canvas (comet logo.tsx `CELLS`; shared by the static mark asset and the
/// animated [`comet_mark_loader`]).
pub const MARK_CELLS: [(f32, f32); 34] = [
    (0., 600.), (0., 720.), (240., 840.), (240., 720.), (120., 840.), (120., 600.), (240., 600.),
    (0., 480.), (0., 360.), (480., 840.), (480., 720.), (120., 360.), (120., 240.), (240., 360.),
    (600., 720.), (480., 600.), (360., 360.), (240., 240.), (600., 600.), (720., 600.), (720., 480.),
    (240., 120.), (600., 380.), (720., 240.), (720., 0.), (480., 240.), (480., 0.), (120., 480.),
    (240., 480.), (360., 840.), (360., 720.), (360., 600.), (360., 480.), (120., 720.),
];

/// Fraction of the pulse cycle the light sweep occupies (comet-loader.tsx `SPREAD`).
pub const MARK_SPREAD: f32 = 0.55;

/// Per-cell stagger fraction along the comet's flight axis — tail tip
/// `(720, 0)` leads, head `(0, 840)` trails (comet-loader.tsx `delayFor`,
/// normalized into the repeating animation's phase space).
pub fn mark_cell_stagger(x: f32, y: f32) -> f32 {
    let t = (820.0 - x + y) / 1660.0;
    (1.0 - t) * MARK_SPREAD
}

/// The animated comet mark (comet-loader.tsx `CometLoader`): the full logo
/// pixel grid with a light wave sweeping tail→head. Each cell rests dim
/// (opacity 0.08, scale 0.9) and flares to full as the crest passes; per-cell
/// stagger follows the flight axis. `height_px` sets the mark's height (width
/// follows the 820:940 canvas).
pub fn comet_mark_loader(id: &'static str, theme: &Theme, height_px: f32) -> impl IntoElement {
    let color = theme.text;
    let scale = height_px / 940.0;
    let cell = 100.0 * scale;
    div()
        .relative()
        .w(px(820.0 * scale))
        .h(px(height_px))
        .children(MARK_CELLS.iter().enumerate().map(move |(i, &(x, y))| {
            let stagger = mark_cell_stagger(x, y);
            // Fixed slot; the animated cell breathes inside it (paint-local).
            div()
                .absolute()
                .left(px(x * scale))
                .top(px(y * scale))
                .size(px(cell))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .rounded(px(16.0 * scale))
                        .bg(color)
                        .size(px(cell))
                        .with_animation((id, i), COMET_PULSE.repeating(), move |el, delta| {
                            // Negative CSS delay ⇒ the cell starts mid-cycle:
                            // the stagger ADDS phase (comet-loader.tsx delayFor).
                            let phase = (delta + stagger).rem_euclid(1.0);
                            el.opacity(motion::pulse_opacity(phase))
                                .size(px(cell * motion::pulse_scale(phase)))
                        }),
                )
        }))
}

/// The comet wave loader: a row of cells pulsing opacity 0.08→1 / scale 0.9→1
/// over 2.4s with a 0.15s stagger per cell.
///
/// `id` scopes the per-cell animation state — give each loader instance a
/// distinct id.
pub fn comet_loader(id: &'static str, theme: &Theme, cell_px: f32) -> impl IntoElement {
    let color = theme.text;
    let slot = cell_px;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(slot / 2.0))
        .children((0..COMET_CELLS).map(move |i| {
            // Fixed slot; the animated cell breathes inside it.
            div()
                .size(px(slot))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .rounded(px(slot / 4.0))
                        .bg(color)
                        .size(px(slot))
                        .with_animation((id, i), COMET_PULSE.repeating(), move |el, delta| {
                            let phase = motion::staggered_phase(delta, i, PULSE_STAGGER);
                            el.opacity(motion::pulse_opacity(phase))
                                .size(px(slot * motion::pulse_scale(phase)))
                        }),
                )
        }))
}

/// Per-row tints of the gradient matrix spinner — comet's "sunrise" gradient
/// (gradient-spin.tsx SUNRISE) sampled at each row: cool blue at the top,
/// through amber, to pink at the bottom.
pub const GSPIN_ROW_TINTS: [u32; MATRIX_SIDE] = [0xB6D3EF, 0xEDB185, 0xF888A0];
/// Opacity a cell rests at between pulses.
pub const GSPIN_DIM: f32 = 0.1;

/// The gradient matrix spinner (WorkingIndicator), ported from comet's
/// gradient-spin.tsx: a 3×3 grid of round cells tinted per row from the
/// sunrise gradient. Each cell pulses opacity once per 750ms period; the
/// per-cell phase follows the "arrow-up" pattern (the pulse enters at the
/// bottom edge and converges toward the top-center cell), so the wave reads
/// as travelling upward.
pub fn gradient_spinner(id: &'static str, _theme: &Theme, cell_px: f32) -> impl IntoElement {
    let center = (MATRIX_SIDE as f32 - 1.0) / 2.0;
    let max = MATRIX_SIDE as f32 - 1.0 + center;
    div()
        .flex()
        .flex_col()
        .gap(px(cell_px / 2.0))
        .children((0..MATRIX_SIDE).map(move |row| {
            let tint: gpui::Hsla = gpui::rgb(GSPIN_ROW_TINTS[row]).into();
            div()
                .flex()
                .flex_row()
                .gap(px(cell_px / 2.0))
                .children((0..MATRIX_SIDE).map(move |col| {
                    let cell_ix = row * MATRIX_SIDE + col;
                    // Distance of this cell from the wave origin, normalized
                    // into a phase offset (gradient-spin's `--gspin-phase`).
                    let d = MATRIX_SIDE as f32 - 1.0 - row as f32 + (col as f32 - center).abs();
                    let phase = if max == 0.0 { 0.0 } else { d / (max + 1.0) };
                    div()
                        .size(px(cell_px))
                        .rounded(px(cell_px / 2.0))
                        .bg(tint)
                        .with_animation(
                            (id, cell_ix),
                            GRADIENT_SPIN.repeating(),
                            move |el, delta| {
                                el.opacity(motion::gspin_opacity(delta + phase, GSPIN_DIM))
                            },
                        )
                }))
        }))
}

/// A 2×3 miniature of [`gradient_spinner`] sized for a status-dot slot
/// (sessions-sidebar working rows): same row tints and pulse timing, but the
/// brightness SNAKES around the grid's perimeter (every cell of a 2×3 grid is
/// on the ring) instead of sweeping as a vertical wave — a tiny radial chase.
/// ~6×10px footprint at the default 2.5px cells.
pub fn mini_gradient_spinner(key: impl Into<SharedString>, cell_px: f32) -> impl IntoElement {
    const COLS: usize = 2;
    const ROWS: usize = 3;
    /// Clockwise ring position of each `(row, col)` cell, top-left first:
    /// (0,0) → (0,1) → (1,1) → (2,1) → (2,0) → (1,0).
    const RING: [[usize; COLS]; ROWS] = [[0, 1], [5, 2], [4, 3]];
    const RING_LEN: f32 = (COLS * ROWS) as f32;
    let key = key.into();
    div()
        .flex()
        .flex_col()
        .gap(px(cell_px / 2.0))
        .children((0..ROWS).map(move |row| {
            let tint: gpui::Hsla = gpui::rgb(GSPIN_ROW_TINTS[row]).into();
            let key = key.clone();
            div()
                .flex()
                .flex_row()
                .gap(px(cell_px / 2.0))
                .children((0..COLS).map(move |col| {
                    let cell_ix = row * COLS + col;
                    let phase = RING[row][col] as f32 / RING_LEN;
                    div()
                        .size(px(cell_px))
                        .rounded(px(cell_px / 2.0))
                        .bg(tint)
                        .with_animation(
                            SharedString::from(format!("{key}-{cell_ix}")),
                            GRADIENT_SPIN.repeating(),
                            move |el, delta| {
                                el.opacity(motion::gspin_opacity(delta + phase, GSPIN_DIM))
                            },
                        )
                }))
        }))
}

/// Full-window boot splash (comet App.tsx `Splash`): the animated comet mark
/// (`h-16`) over the app background with an uppercase tracked "Loading" line.
/// While `fading` it plays `splash-out` (150ms hold, then 0.5s fade + 6px
/// lift); the shell removes it once [`SPLASH_OUT`] has run its course.
pub fn splash_overlay(theme: &Theme, fading: bool) -> AnyElement {
    let content = div()
        .absolute()
        .inset_0()
        .bg(theme.bg)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(28.0))
        .child(comet_mark_loader("boot-splash", theme, 64.0))
        .child(loading_word(theme));
    if fading {
        motion::splash_out("boot-splash-out", content).into_any_element()
    } else {
        content.into_any_element()
    }
}

/// "L O A D I N G" — `text-[11px] uppercase tracking-[0.32em]
/// text-muted-foreground/70`; tracking approximated with thin spaces (gpui has
/// no letter-spacing at the pinned rev).
pub fn loading_word(theme: &Theme) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(theme.text_muted.opacity(0.7))
        .child(SharedString::from("L\u{2009}O\u{2009}A\u{2009}D\u{2009}I\u{2009}N\u{2009}G"))
}

// Compile-time proof the specs referenced here stay wired to the catalog.
const _: () = {
    assert!(SPLASH_OUT.delay_ms == 150);
    assert!(COMET_PULSE.duration_ms == 2400);
    assert!(GRADIENT_SPIN.duration_ms == 750);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_stagger_follows_flight_axis() {
        // Tail tip (720, 0) leads: near-maximal stagger (starts deepest into
        // the cycle); head (0, 840) trails with stagger 0.
        let tail = mark_cell_stagger(720.0, 0.0);
        let head = mark_cell_stagger(0.0, 840.0);
        assert!(tail > head, "tail {tail} should lead head {head}");
        assert!((head - 0.0).abs() < 1e-6, "head stagger ≈ 0, got {head}");
        assert!(tail <= MARK_SPREAD + 1e-6, "stagger capped at SPREAD");
        // Every logo cell stays inside [0, SPREAD].
        for &(x, y) in &MARK_CELLS {
            let s = mark_cell_stagger(x, y);
            assert!((0.0..=MARK_SPREAD + 1e-6).contains(&s), "cell ({x},{y}) stagger {s}");
        }
    }
}
