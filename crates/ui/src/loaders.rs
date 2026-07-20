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
use crate::theme::{self, Theme};

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

/// The gradient matrix spinner (WorkingIndicator): a 3×3 grid whose cells run a
/// 750ms phase wave along the diagonals, painting a moving accent→text gradient.
pub fn gradient_spinner(id: &'static str, theme: &Theme, cell_px: f32) -> impl IntoElement {
    let hot = theme.accent;
    let cold = theme.text_faint;
    let wave_count = MATRIX_SIDE * 2 - 1; // diagonals of the matrix
    div()
        .flex()
        .flex_col()
        .gap(px(cell_px / 2.0))
        .children((0..MATRIX_SIDE).map(move |row| {
            div()
                .flex()
                .flex_row()
                .gap(px(cell_px / 2.0))
                .children((0..MATRIX_SIDE).map(move |col| {
                    let diagonal = row + col;
                    let cell_ix = row * MATRIX_SIDE + col;
                    div()
                        .size(px(cell_px))
                        .rounded(px(1.5))
                        .bg(cold)
                        .with_animation(
                            (id, cell_ix),
                            GRADIENT_SPIN.repeating(),
                            move |el, delta| {
                                let w = motion::matrix_wave(delta, diagonal, wave_count);
                                el.bg(theme::mix(cold, hot, w)).opacity(0.25 + 0.75 * w)
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
