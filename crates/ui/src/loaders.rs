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

/// Full-window boot splash: loader + wordmark over the app background.
/// While `fading` it plays `splash-out` (150ms hold, then 0.5s fade + 6px lift);
/// the shell removes it once [`SPLASH_OUT`] has run its course.
pub fn splash_overlay(theme: &Theme, fading: bool) -> AnyElement {
    let content = div()
        .absolute()
        .inset_0()
        .bg(theme.bg)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(Theme::SPACE_LG))
        .child(comet_loader("boot-splash", theme, 8.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("comet")),
        );
    if fading {
        motion::splash_out("boot-splash-out", content).into_any_element()
    } else {
        content.into_any_element()
    }
}

// Compile-time proof the specs referenced here stay wired to the catalog.
const _: () = {
    assert!(SPLASH_OUT.delay_ms == 150);
    assert!(COMET_PULSE.duration_ms == 2400);
    assert!(GRADIENT_SPIN.duration_ms == 750);
};
