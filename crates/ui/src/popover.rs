//! Popover / menu primitives: an anchored floating layer with the `menu-in`
//! animation, outside-click dismissal, and pure keyboard-navigation + search
//! reducers shared by every picker and menu (feature-inventory §1.12 popovers).
//!
//! gpui pattern (examples/popover.rs at the pinned rev): the trigger element
//! conditionally children a `deferred(anchored().child(content))` — deferred
//! paints on a floating layer above everything, anchored positions it relative
//! to the trigger (or an explicit point for context menus).
//!
//! Pure logic (wrap-around list navigation, ranked substring filtering, key
//! classification) lives in free functions with unit tests; the elements only
//! feed them measurements/events.

use gpui::{Anchor, AnyElement, ElementId, IntoElement, Pixels, Point, SharedString, div, prelude::*, px};

use crate::motion::{self, AnimationExt as _, COMET_PULSE};
use crate::theme::{Theme, grey, white_alpha};

// ---------------------------------------------------------------------------
// Loadable — async slot state shared by pickers/settings pages
// ---------------------------------------------------------------------------

/// One async-loaded slot: `Idle` (never requested) → `Loading` (skeletons) →
/// `Ready` / `Error` (inline message + Retry).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Loadable<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Error(String),
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Loadable::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading)
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Loadable::Error(message) => Some(message),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure reducers
// ---------------------------------------------------------------------------

/// Step the active row of a menu: wraps at both ends; `None` enters at the
/// edge matching the direction. Empty menus stay `None`.
pub fn menu_step(active: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let count_i = count as isize;
    let next = match active {
        None => {
            if delta >= 0 {
                0
            } else {
                count_i - 1
            }
        }
        Some(at) => (at as isize + delta).rem_euclid(count_i),
    };
    Some(next as usize)
}

/// Match rank of a label against a query: `0` prefix match, `1` substring,
/// `None` no match. Case-insensitive; an empty query matches everything at
/// rank 1 (input order preserved).
pub fn match_rank(query: &str, label: &str) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let label = label.to_lowercase();
    if label.starts_with(&query) {
        Some(0)
    } else if label.contains(&query) {
        Some(1)
    } else {
        None
    }
}

/// Filter + rank labels for a search query: prefix matches first, then
/// substring matches, stable within each rank. Returns indices into `labels`.
pub fn filter_indices<S: AsRef<str>>(query: &str, labels: &[S]) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(ix, label)| match_rank(query, label.as_ref()).map(|rank| (rank, ix)))
        .collect();
    ranked.sort_by_key(|&(rank, ix)| (rank, ix));
    ranked.into_iter().map(|(_, ix)| ix).collect()
}

/// Keys the pickers care about, classified from a raw keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    /// Plain Enter — activate the highlighted row.
    Enter,
    /// Cmd/Ctrl+Enter — the "pick this folder" accelerator in the browser.
    ModEnter,
    Escape,
    Backspace,
    Other,
}

pub fn classify_key(key: &str, cmd: bool, ctrl: bool) -> MenuKey {
    match key {
        "up" => MenuKey::Up,
        "down" => MenuKey::Down,
        "enter" if cmd || ctrl => MenuKey::ModEnter,
        "enter" => MenuKey::Enter,
        "escape" => MenuKey::Escape,
        "backspace" => MenuKey::Backspace,
        _ => MenuKey::Other,
    }
}

// ---------------------------------------------------------------------------
// Elements
// ---------------------------------------------------------------------------

/// The floating-menu surface (comet `.glass-surface` + `menuSurface`):
/// `rounded-xl border border-white/[0.1] p-1` over the frosted glass tint.
/// gpui has no backdrop blur at the pinned rev, so the glass
/// (`oklch(0.33 0 0 / 34%)` over blurred dark content) is approximated with
/// the near-opaque tone it composites to on the dark panels (~#161616), plus
/// the same hairline + baked-in shadow.
pub fn popover_card(theme: &Theme) -> gpui::Div {
    div()
        .bg(grey(0x16))
        .border_1()
        .border_color(white_alpha(0.10))
        .rounded(px(12.0))
        .shadow_lg()
        .p(px(4.0))
        .overflow_hidden()
        .text_size(px(13.0))
        .text_color(theme.text)
}

/// [`popover_card`] without the `p-1` inset — for popovers that manage their
/// own internal panes (the harness/model picker's rail + list split).
pub fn popover_card_flush(theme: &Theme) -> gpui::Div {
    popover_card(theme).p(px(0.0))
}

/// Pin a floating layer's origin to the trigger's top-left. The anchored
/// element is absolutely positioned; without explicit insets its *static*
/// position is subject to the trigger's own flex alignment (an `items_center`
/// trigger would vertically center the whole floating layer). A zero-size
/// absolutely-inset wrapper fixes the origin at the corner.
fn pinned_layer(layer: AnyElement) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(layer)
        .into_any_element()
}

/// Wrap popover content in a floating anchored layer attached to the trigger:
/// the caller `.child(anchored_menu(...))`s this from the trigger element while
/// open. Plays `menu-in` (0.14s fade + 2px drop). Dismissal is the caller's
/// `.on_mouse_down_out` on the content.
pub fn anchored_menu(id: impl Into<ElementId>, content: AnyElement) -> AnyElement {
    pinned_layer(
        gpui::deferred(
            gpui::anchored()
                .anchor(Anchor::TopLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(motion::menu_in(id, div().pt(px(6.0)).child(content))),
        )
        .priority(1)
        .into_any_element(),
    )
}

/// [`anchored_menu`] opening UPWARD from the trigger (composer pickers, the
/// user menu — anything anchored near the window bottom; Radix flips these
/// automatically, gpui's `anchored` needs the side picked).
pub fn anchored_menu_above(id: impl Into<ElementId>, content: AnyElement) -> AnyElement {
    pinned_layer(
        gpui::deferred(
            gpui::anchored()
                .anchor(Anchor::BottomLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(motion::menu_in(id, div().pb(px(6.0)).child(content))),
        )
        .priority(1)
        .into_any_element(),
    )
}

/// [`anchored_menu_above`] right-aligned to the trigger's right edge (t3code
/// ComboboxPopup `align="end"` — right-side triggers like the composer's ref
/// picker open leftward instead of running off the window).
pub fn anchored_menu_above_end(id: impl Into<ElementId>, content: AnyElement) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .right_0()
        .size_0()
        .child(
            gpui::deferred(
                gpui::anchored()
                    .anchor(Anchor::BottomRight)
                    .snap_to_window_with_margin(px(8.0))
                    .child(motion::menu_in(id, div().pb(px(6.0)).child(content))),
            )
            .priority(1)
            .into_any_element(),
        )
        .into_any_element()
}

/// A floating menu at an explicit window position (context menus).
pub fn menu_at(
    id: impl Into<ElementId>,
    position: Point<Pixels>,
    content: AnyElement,
) -> AnyElement {
    gpui::deferred(
        gpui::anchored()
            .position(position)
            .anchor(Anchor::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(motion::menu_in(id, div().child(content))),
    )
    .priority(1)
    .into_any_element()
}

/// Full-window modal: dim scrim + centered card with the `dialog-in` entrance.
/// The scrim swallows clicks; the caller wires its own dismiss/confirm.
/// `viewport` is the window size (an `anchored` layer sizes to its children,
/// so the scrim needs explicit dimensions).
pub fn modal(
    id: impl Into<ElementId>,
    viewport: gpui::Size<Pixels>,
    card: AnyElement,
) -> AnyElement {
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::dialog_in(id, div().child(card))),
            ),
    )
    .priority(2)
    .into_any_element()
}

/// One menu row (comet `menuItem`): `gap-2.5 rounded-lg px-2 py-1.5
/// text-[13px]`, active = `bg-white/10 text-foreground`, hover wash
/// `white/[0.08]` fading over `transition-colors` (floating-styles.ts) via the
/// per-`fade_key` [`motion::hover_blend`]. The caller adds the id/click
/// listener — `fade_key` must be unique app-wide and stable across frames
/// (the id string is a good choice).
pub fn menu_row(theme: &Theme, active: bool, fade_key: impl Into<SharedString>) -> gpui::Div {
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .text_size(px(13.0))
        .cursor_pointer();
    if active {
        row.bg(white_alpha(0.10)).text_color(theme.text)
    } else {
        let fade_key = fade_key.into();
        let mut row = row
            .text_color(motion::hover_blend(
                &fade_key,
                theme.text.opacity(0.9),
                Theme::dark().text,
            ))
            .bg(motion::hover_blend(
                &fade_key,
                gpui::transparent_black(),
                white_alpha(0.08),
            ));
        // Imperative form — the caller's `.id(...)` makes the element stateful
        // (hover listeners need element state, `.on_hover` needs `Stateful`).
        row.interactivity().on_hover(motion::hover_listener(fade_key));
        row
    }
}

/// [`menu_row`] with a distinct keyboard-navigation highlight: a selected row
/// carries the full `bg-white/10` wash, the keyboard cursor the lighter
/// `bg-white/[0.08]` (comet's `data-[highlighted]` styling) — two selected-
/// looking rows never appear at once.
pub fn menu_row_nav(
    theme: &Theme,
    selected: bool,
    highlighted: bool,
    fade_key: impl Into<SharedString>,
) -> gpui::Div {
    let row = menu_row(theme, selected, fade_key);
    if !selected && highlighted {
        row.bg(white_alpha(0.08)).text_color(theme.text)
    } else {
        row
    }
}

/// Small uppercase section heading inside a floating menu (comet
/// `MenuHeading`): `px-2 pb-1 pt-1.5 text-[10px] font-medium uppercase
/// tracking-[0.1em] text-muted-foreground/60`. gpui has no letter-spacing at
/// the pinned rev; the tracking is approximated with hair spaces.
pub fn menu_heading(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(8.0))
        .pb(px(4.0))
        .pt(px(6.0))
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from(tracked_upper(label)))
}

/// Uppercase + hair-space tracking (see [`menu_heading`]).
pub fn tracked_upper(label: &str) -> String {
    let upper = label.to_uppercase();
    let mut out = String::with_capacity(upper.len() * 2);
    let mut first = true;
    for ch in upper.chars() {
        if !first {
            out.push('\u{200A}'); // hair space ≈ 0.1em tracking
        }
        out.push(ch);
        first = false;
    }
    out
}

/// Hairline divider between menu sections (comet `MenuSeparator`:
/// `mx-1 my-1 h-px bg-white/[0.07]`).
pub fn menu_separator() -> gpui::Div {
    div().h(px(1.0)).mx(px(4.0)).my(px(4.0)).bg(white_alpha(0.07))
}

/// The trailing check on the selected row (comet `MenuCheck`): 14px,
/// `text-foreground/70`, pushed to the row end by the caller's flex.
pub fn menu_check(theme: &Theme) -> impl IntoElement {
    crate::icons::icon(crate::icons::CHECK)
        .size(px(14.0))
        .text_color(theme.text.opacity(0.7))
}

/// A muted kbd hint chip inside menu rows (`⌘↵`-style accelerators).
pub fn kbd_hint(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(5.0))
        .bg(white_alpha(0.05))
        .text_size(px(10.0))
        .font_family(Theme::dark().font_mono.clone())
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from(label.to_string()))
}

/// The search/text input frame at the top of a picker popover (comet
/// `searchInput`: `w-full rounded-lg bg-white/[0.04] px-2.5 py-1.5
/// text-[13px]` + `mb-1`, borderless — full width inside the card's own
/// p-1, only a 4px bottom margin).
pub fn search_input_frame(_theme: &Theme, input: AnyElement) -> gpui::Div {
    div()
        .mb(px(4.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(white_alpha(0.04))
        .text_size(px(13.0))
        .child(input)
}

/// A bordered trailing menu section (comet picker action groups /
/// branch-picker worktree block: `mt-1 flex flex-col gap-0.5 border-t
/// border-white/[0.06] pt-1` — the hairline runs edge-to-edge of the card's
/// p-1 inset, unlike [`menu_separator`]'s mx-1).
pub fn menu_section() -> gpui::Div {
    div()
        .mt(px(4.0))
        .pt(px(4.0))
        .border_t_1()
        .border_color(white_alpha(0.06))
        .flex()
        .flex_col()
        .gap(px(2.0))
}

// ---------------------------------------------------------------------------
// Dialog primitives (comet dialog.tsx / sidebar dialogs.tsx)
// ---------------------------------------------------------------------------

/// The centered dialog card (`dialog-pop`): `w-[360px] rounded-2xl border
/// border-white/[0.1] bg-popover/95 p-5 shadow-2xl` — popover tone ≈ #101010.
pub fn dialog_card(theme: &Theme) -> gpui::Div {
    div()
        .w(px(360.0))
        .p(px(20.0))
        .rounded(px(16.0))
        .bg(grey(0x10))
        .border_1()
        .border_color(white_alpha(0.10))
        .shadow_lg()
        .flex()
        .flex_col()
        .text_color(theme.text)
}

/// Dialog title: `text-[15px] font-semibold tracking-tight`.
pub fn dialog_title(theme: &Theme, title: &str) -> gpui::Div {
    div()
        .text_size(px(15.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(SharedString::from(title.to_string()))
}

/// Dialog body copy: `text-[13px] leading-relaxed text-muted-foreground`.
pub fn dialog_body(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(13.0))
        .line_height(px(19.0))
        .text_color(theme.text_muted)
        .child(copy.into())
}

/// Dialog text-field frame: `rounded-lg border border-white/[0.08]
/// bg-white/[0.04] px-3 py-2 text-[14px]`.
pub fn dialog_field(input: AnyElement) -> gpui::Div {
    div()
        .w_full()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(white_alpha(0.08))
        .bg(white_alpha(0.04))
        .text_size(px(14.0))
        .child(input)
}

/// Ghost button (`btnGhost`): quiet text, hover wash fading over
/// `transition-colors` (comet dialogs.tsx). Caller adds id + click; `fade_key`
/// as in [`menu_row`].
pub fn btn_ghost(theme: &Theme, label: &str, fade_key: impl Into<SharedString>) -> gpui::Div {
    let fade_key = fade_key.into();
    let mut btn = div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .text_size(px(13.0))
        .text_color(motion::hover_blend(
            &fade_key,
            theme.text_muted,
            Theme::dark().text,
        ))
        .bg(motion::hover_blend(
            &fade_key,
            gpui::transparent_black(),
            white_alpha(0.06),
        ))
        .cursor_pointer()
        .child(SharedString::from(label.to_string()));
    btn.interactivity().on_hover(motion::hover_listener(fade_key));
    btn
}

/// Primary button (`btnPrimary`): white fill, near-black text.
pub fn btn_primary(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(theme.text)
        .text_size(px(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(grey(0x0e))
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(SharedString::from(label.to_string()))
}

/// Destructive button (`btnDestructive`): the muted red fill.
pub fn btn_danger(_theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(crate::theme::oklch(0.58, 0.16, 25.0))
        .text_size(px(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::white())
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(SharedString::from(label.to_string()))
}

/// Pulsing skeleton rows shown while a list loads (comet:
/// `h-7 animate-pulse rounded-md bg-white/[0.04]`).
pub fn skeleton_rows(id: &'static str, _theme: &Theme, count: usize) -> AnyElement {
    let wash = white_alpha(0.04);
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .py(px(4.0))
        .children((0..count).map(move |i| {
            div()
                .h(px(28.0))
                .rounded(px(Theme::CONTROL_RADIUS))
                .bg(wash)
                .with_animation((id, i), COMET_PULSE.repeating(), move |el, delta| {
                    let phase = motion::staggered_phase(delta, i, 0.08);
                    el.opacity(0.35 + 0.4 * motion::pulse_wave(phase))
                })
        }))
        .into_any_element()
}

/// Inline error row + Retry affordance (the caller attaches the listener to the
/// returned id).
pub fn error_row(theme: &Theme, message: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(Theme::SPACE_SM))
        .text_size(px(12.0))
        .text_color(theme.danger)
        .child(gpui::SharedString::from(message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_step_wraps_and_enters() {
        // Entering an empty menu stays out.
        assert_eq!(menu_step(None, 0, 1), None);
        assert_eq!(menu_step(Some(3), 0, 1), None);
        // Entering from nothing lands on the matching edge.
        assert_eq!(menu_step(None, 3, 1), Some(0));
        assert_eq!(menu_step(None, 3, -1), Some(2));
        // Stepping wraps both ways.
        assert_eq!(menu_step(Some(2), 3, 1), Some(0));
        assert_eq!(menu_step(Some(0), 3, -1), Some(2));
        assert_eq!(menu_step(Some(1), 3, 1), Some(2));
    }

    #[test]
    fn filter_ranks_prefix_before_substring() {
        let labels = ["main", "feature/main-sync", "master", "dev"];
        // Prefix matches ("main", "master") come before the substring match.
        assert_eq!(filter_indices("ma", &labels), vec![0, 2, 1]);
        // Case-insensitive.
        assert_eq!(filter_indices("MA", &labels), vec![0, 2, 1]);
        // No matches → empty.
        assert!(filter_indices("zzz", &labels).is_empty());
        // Empty / whitespace query keeps input order.
        assert_eq!(filter_indices("", &labels), vec![0, 1, 2, 3]);
        assert_eq!(filter_indices("   ", &labels), vec![0, 1, 2, 3]);
    }

    #[test]
    fn match_rank_kinds() {
        assert_eq!(match_rank("re", "release"), Some(0));
        assert_eq!(match_rank("lease", "release"), Some(1));
        assert_eq!(match_rank("x", "release"), None);
        assert_eq!(match_rank("", "anything"), Some(1));
    }

    #[test]
    fn key_classification() {
        assert_eq!(classify_key("up", false, false), MenuKey::Up);
        assert_eq!(classify_key("down", false, false), MenuKey::Down);
        assert_eq!(classify_key("enter", false, false), MenuKey::Enter);
        assert_eq!(classify_key("enter", true, false), MenuKey::ModEnter);
        assert_eq!(classify_key("enter", false, true), MenuKey::ModEnter);
        assert_eq!(classify_key("escape", false, false), MenuKey::Escape);
        assert_eq!(classify_key("backspace", false, false), MenuKey::Backspace);
        assert_eq!(classify_key("a", false, false), MenuKey::Other);
    }

    #[test]
    fn tracked_upper_spaces_letters() {
        assert_eq!(tracked_upper("ab"), "A\u{200A}B");
        assert_eq!(tracked_upper("Question"), "Q\u{200A}U\u{200A}E\u{200A}S\u{200A}T\u{200A}I\u{200A}O\u{200A}N");
        assert_eq!(tracked_upper(""), "");
    }

    #[test]
    fn loadable_accessors() {
        let l: Loadable<u32> = Loadable::Ready(7);
        assert_eq!(l.ready(), Some(&7));
        assert!(!l.is_loading());
        let e: Loadable<u32> = Loadable::Error("boom".into());
        assert_eq!(e.error(), Some("boom"));
        assert!(Loadable::<u32>::Loading.is_loading());
        assert_eq!(Loadable::<u32>::default(), Loadable::Idle);
    }
}
