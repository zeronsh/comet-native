//! Shared scaffolding for the settings pages — the original's page rhythm
//! (`mx-auto max-w-3xl px-6 pb-16 pt-8`), section cards, row layout, badges
//! and small buttons, so every page reads as the same product surface
//! (comet settings.devices.tsx / settings.agents.tsx / settings.archived.tsx).

use gpui::{AnyElement, SharedString, div, prelude::*, px};

use crate::theme::{Theme, white_alpha};

/// Centered page column: `mx-auto w-full max-w-3xl px-6 pb-16 pt-8`.
pub fn page_column() -> gpui::Div {
    div()
        .w_full()
        .max_w(px(768.0))
        .mx_auto()
        .px(px(24.0))
        .pt(px(32.0))
        .pb(px(64.0))
        .flex()
        .flex_col()
}

/// Page headline row: `flex items-baseline gap-2.5` — `text-base font-semibold`
/// title + `text-[13px]` count sharing a baseline (comet settings.devices.tsx).
pub fn page_header(theme: &Theme, title: &str, count: Option<usize>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .when_some(count, |el, count| {
            el.child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(format!("{count}"))),
            )
        })
}

/// Subtitle under the headline: `mt-1 text-[13px] text-muted-foreground`.
pub fn page_subtitle(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt(px(4.0))
        .text_size(px(13.0))
        .text_color(theme.text_muted)
        .child(copy.into())
}

/// Section card: `mt-6 overflow-hidden rounded-xl border border-border bg-card`
/// — the opaque raised-card tone (comet `--card`), not a translucent wash.
pub fn section_card(theme: &Theme) -> gpui::Div {
    div()
        .mt(px(24.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .overflow_hidden()
        .flex()
        .flex_col()
}

/// One card row: `border-t border-border px-5 py-3.5 first:border-t-0` with the
/// quiet hover wash.
pub fn card_row(theme: &Theme, first: bool) -> gpui::Div {
    div()
        .px(px(20.0))
        .py(px(14.0))
        .when(!first, |el| el.border_t_1().border_color(theme.border))
        .hover(|s| s.bg(white_alpha(0.015)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.0))
}

/// The identity tile on a row: `size-9 rounded-[10px] border bg-white/[0.03]`
/// around a 16px icon.
pub fn row_tile(theme: &Theme, icon_path: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .size(px(36.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border)
        .bg(white_alpha(0.03))
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(icon_path)
                .size(px(16.0))
                .text_color(theme.text_muted),
        )
}

/// Row title: `text-[13.5px] font-medium leading-tight`.
pub fn row_title(theme: &Theme, title: impl Into<SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(px(13.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(title.into())
}

/// The quiet meta line under a row title: `text-[11.5px]
/// text-muted-foreground/65` fragments joined by dots.
pub fn meta_line(theme: &Theme, fragments: Vec<AnyElement>) -> gpui::Div {
    let mut line = div()
        .mt(px(4.0))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .gap_y(px(2.0))
        .text_size(px(11.5))
        .text_color(theme.text_muted.opacity(0.65));
    let mut first = true;
    for fragment in fragments {
        if !first {
            line = line.child(
                div()
                    .text_color(theme.text_muted.opacity(0.3))
                    .child(SharedString::from("·")),
            );
        }
        line = line.child(fragment);
        first = false;
    }
    line
}

/// Right-anchored badge pill: `rounded-full border px-2 py-0.5 text-[10.5px]`.
pub fn badge(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_size(px(10.5))
        .text_color(theme.text_muted)
        .child(label.into())
}

/// Emerald status pill (the Accounts "Active" badge:
/// `bg-emerald-400/[0.12] text-emerald-300/90`).
pub fn badge_active(label: impl Into<SharedString>) -> gpui::Div {
    let emerald = crate::theme::oklch(0.765, 0.177, 163.223);
    let emerald_text = crate::theme::oklch(0.845, 0.143, 164.978); // emerald-300
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .bg(emerald.opacity(0.12))
        .text_size(px(10.5))
        .text_color(emerald_text.opacity(0.9))
        .child(label.into())
}

/// A small quiet ghost action (`rounded-lg px-2.5 py-1.5 text-[12px]
/// text-muted-foreground`). Caller adds id + click + leading icon child AND
/// its own `.hover(..)` — gpui panics on a second hover, and the pages vary
/// it (reveal opacity, 4% vs 6% washes).
pub fn ghost_action(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .rounded(px(8.0))
        .px(px(10.0))
        .py(px(6.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .cursor_pointer()
}

/// The default ghost-action hover wash (`hover:bg-white/[0.06]
/// hover:text-foreground`).
pub fn ghost_hover(s: gpui::StyleRefinement) -> gpui::StyleRefinement {
    s.bg(white_alpha(0.06)).text_color(Theme::dark().text)
}

/// The dismissible red error strip (`flex items-start gap-2 rounded-xl border
/// border-red-400/20 bg-red-400/[0.06] text-red-300/90` with a leading
/// `DangerTriangle mt-0.5 size-4`).
pub fn error_strip(message: impl Into<SharedString>) -> gpui::Div {
    let red = crate::theme::oklch(0.704, 0.191, 22.216); // red-400
    let red_text = crate::theme::oklch(0.81, 0.108, 19.6); // red-300
    div()
        .mt(px(16.0))
        .px(px(16.0))
        .py(px(12.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(red.opacity(0.2))
        .bg(red.opacity(0.06))
        .text_size(px(12.5))
        .text_color(red_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(16.0))
                    .text_color(red_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}

/// The amber warning strip (`flex items-start gap-2 border-amber-400/20
/// bg-amber-400/[0.06] text-amber-200/90` with a leading `DangerTriangle
/// mt-0.5 size-3.5`).
pub fn warning_strip(message: impl Into<SharedString>) -> gpui::Div {
    let amber = crate::theme::oklch(0.828, 0.189, 84.429); // amber-400
    let amber_text = crate::theme::oklch(0.924, 0.12, 95.746); // amber-200
    div()
        .mt(px(8.0))
        .px(px(16.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(amber.opacity(0.2))
        .bg(amber.opacity(0.06))
        .text_size(px(12.0))
        .text_color(amber_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(14.0))
                    .text_color(amber_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}
