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

use gpui::{
    Anchor, AnyElement, Context, Div, ElementId, IntoElement, Pixels, Point, SharedString, Window,
    div, prelude::*, px,
};

use crate::motion::{self, ZERON_PULSE};
use crate::theme::{Theme, hairline, ink};

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
// Popup — open/closing/closed lifecycle (exit animations)
// ---------------------------------------------------------------------------

/// Popup state with an exit phase. gpui unmounts an element the frame its
/// state drops, so a closing animation needs the state held alive while
/// [`motion::menu_out`] plays: `open` → `begin_close` (render keeps mounting,
/// with the out animation and dead hit-testing) → [`reap_popup`]'s timer
/// `finish_close`es ~[`motion::MENU_OUT`] later. Use [`Self::is_open`] for
/// logic (a closing popup already reads as closed) and [`Self::get`] /
/// [`Self::is_closing`] for rendering.
pub struct Popup<T> {
    /// `Some((state, closing_since))` while mounted; `closing_since` is the
    /// exit-phase start.
    inner: Option<(T, Option<std::time::Instant>)>,
    /// Whether the popup was still mounted when the current trigger press
    /// began — see [`Self::note_trigger_press`].
    pressed_while_open: bool,
}

impl<T> Default for Popup<T> {
    fn default() -> Self {
        Self {
            inner: None,
            pressed_while_open: false,
        }
    }
}

impl<T> Popup<T> {
    pub fn open(&mut self, value: T) {
        self.inner = Some((value, None));
    }

    /// Open and interactive (not closing).
    pub fn is_open(&self) -> bool {
        matches!(self.inner, Some((_, None)))
    }

    pub fn is_closing(&self) -> bool {
        matches!(self.inner, Some((_, Some(_))))
    }

    /// When the exit phase began — what the render path hands to the popover
    /// wrappers, which derive the eased exit progress from it each frame.
    pub fn closing_since(&self) -> Option<std::time::Instant> {
        match &self.inner {
            Some((_, Some(since))) => Some(*since),
            _ => None,
        }
    }

    /// The state while mounted — open OR playing the exit animation. Render
    /// paths use this; logic paths use [`Self::as_open`]/[`Self::open_mut`].
    pub fn get(&self) -> Option<&T> {
        self.inner.as_ref().map(|(value, _)| value)
    }

    /// The state only while genuinely open — `None` during the exit phase, so
    /// event handlers on a dying popup fall through.
    pub fn as_open(&self) -> Option<&T> {
        match &self.inner {
            Some((value, None)) => Some(value),
            _ => None,
        }
    }

    pub fn open_mut(&mut self) -> Option<&mut T> {
        match &mut self.inner {
            Some((value, None)) => Some(value),
            _ => None,
        }
    }

    /// Enter the exit phase. Returns `true` when this call started it (the
    /// caller then schedules [`reap_popup`]); `false` if already closing or
    /// closed.
    pub fn begin_close(&mut self) -> bool {
        match &mut self.inner {
            Some((_, closing @ None)) => {
                *closing = Some(std::time::Instant::now());
                true
            }
            _ => false,
        }
    }

    /// Record, from the trigger's `on_mouse_down`, whether this popup is
    /// still mounted. The anchored card's `on_mouse_down_out` fires on that
    /// same press and begins the close, so by click (mouse-up) time the
    /// popup already reads as closed — the click handler alone cannot tell
    /// "this press dismissed it; stay closed" from "open fresh", and a
    /// plain toggle closes-and-reopens (user report). Both handler orders
    /// work: open and mid-exit each count as mounted. Every trigger click
    /// is preceded by a trigger mouse-down, so the note is never stale.
    pub fn note_trigger_press(&mut self) {
        self.note_trigger_press_matching(|_| true);
    }

    /// [`Self::note_trigger_press`] for popups whose state distinguishes
    /// which trigger owns them (e.g. one `Popup<PickerKind>` shared by
    /// several triggers): only a press on the OWNING trigger counts, so
    /// clicking a different trigger switches menus instead of swallowing.
    pub fn note_trigger_press_matching(&mut self, owns: impl FnOnce(&T) -> bool) {
        self.pressed_while_open = self.inner.as_ref().is_some_and(|(value, _)| owns(value));
    }

    /// Consume the press note: `true` when the press that produced the
    /// current click found the popup mounted — the click should leave it
    /// closed rather than reopen it.
    pub fn take_press_was_open(&mut self) -> bool {
        std::mem::take(&mut self.pressed_while_open)
    }

    /// Drop the state if the exit phase has run its course. A popup reopened
    /// (or re-closed) since the matching [`begin_close`] is left alone — the
    /// newer phase's own reap handles it.
    pub fn finish_close(&mut self) {
        if let Some((_, Some(since))) = &self.inner
            && since.elapsed() >= motion::MENU_OUT.total().mul_f32(motion::speed_scale())
        {
            self.inner = None;
        }
    }
}

/// Schedule the reap for a [`Popup::begin_close`]: after the exit animation's
/// span, drop the popup state and repaint. `popup` re-borrows the field from
/// the view (the state can't be captured — the view owns it).
pub fn reap_popup<V: 'static, T: 'static>(
    cx: &mut gpui::Context<V>,
    popup: impl Fn(&mut V) -> &mut Popup<T> + 'static,
) {
    cx.spawn(async move |view, cx| {
        cx.background_executor()
            .timer(
                motion::MENU_OUT
                    .total()
                    .mul_f32(motion::speed_scale())
                    .saturating_add(std::time::Duration::from_millis(20)),
            )
            .await;
        view.update(cx, |view, cx| {
            popup(view).finish_close();
            cx.notify();
        })
        .ok();
    })
    .detach();
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
        // Readline/emacs motion: ctrl-n/ctrl-p mirror ↓/↑ in every picker.
        // Safe to claim frame-wide — neither chord is a text-editing binding
        // in the palette keymaps, so they always bubble here unconsumed.
        "n" if ctrl => MenuKey::Down,
        "p" if ctrl => MenuKey::Up,
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

/// The floating-menu surface (zeron `.glass-surface` + `menuSurface`):
/// `rounded-xl border border-white/[0.1] p-1` over the frosted glass tint —
/// the real recipe now that the fork paints backdrop blur: the
/// [`Theme::glass_overlay`] tint (`oklch(0.33 0 0 / 34%)` on dark) over the
/// [`crate::frost::MENU_BLUR`] blur from the mount helpers below, plus the
/// same hairline + baked-in shadow. Opaque platforms keep the near-opaque
/// tone the reference composites to on the dark panels (~#161616).
/// Corner radius of every floating card. The frost wrapper masks its backdrop
/// blur to the same value, so the two must agree.
pub const CARD_RADIUS: f32 = 12.0;

pub fn popover_card(theme: &Theme) -> gpui::Div {
    let card = div()
        .border_1()
        .border_color(hairline(0.10))
        .rounded(px(CARD_RADIUS))
        .shadow_lg()
        .p(px(4.0))
        .overflow_hidden()
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(theme.text);
    if theme.is_frost() {
        // Translucent tint — the backdrop blur beneath it comes from the
        // [`crate::frost::frosted`] wrapper at the mount helpers below.
        card.bg(theme.glass_overlay())
    } else {
        card.bg(theme.surface_overlay)
    }
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

/// Eased exit progress (0..=1) for a [`Popup`] closing instant, computed from
/// the wall clock at render time. Monotonic by construction — unlike the
/// animation element's own clock, it can never replay from 0 mid-exit.
fn exit_progress(since: std::time::Instant) -> f32 {
    let total = motion::MENU_OUT
        .total()
        .mul_f32(motion::speed_scale())
        .as_secs_f32();
    let raw = if total <= 0.0 {
        1.0
    } else {
        (since.elapsed().as_secs_f32() / total).clamp(0.0, 1.0)
    };
    motion::MENU_OUT.progress(raw)
}

/// The frosted card for a popover layer: full blur while open; while exiting
/// the blur radius rides the exit progress down to 0 — the `BackdropBlur`
/// primitive ignores `element_opacity`, so without this the glass slab would
/// hold full strength through the fade and pop off at unmount.
fn frosted_menu(exit: Option<f32>, content: AnyElement) -> AnyElement {
    let blur = crate::frost::MENU_BLUR * (1.0 - exit.unwrap_or(0.0));
    crate::frost::frosted(CARD_RADIUS, blur, content).into_any_element()
}

/// Entrance or exit motion for a popover layer. While exiting (the [`Popup`]
/// closing phase, `exit = Some(progress)`) the content plays
/// [`motion::menu_out`] under a fresh animation id (same-id reuse would
/// inherit the entrance's finished clock and snap to the end state) and gets
/// an occluding overlay on top — the dying menu's rows must not take clicks,
/// and the overlay also keeps stray clicks from reaching whatever sits
/// underneath.
fn menu_motion(id: SharedString, exit: Option<f32>, inner: gpui::Div) -> AnyElement {
    if let Some(t) = exit {
        let inner = inner.relative().child(div().absolute().inset_0().occlude());
        motion::menu_out(SharedString::from(format!("{id}-out")), t, inner).into_any_element()
    } else {
        motion::menu_in(id, inner).into_any_element()
    }
}

/// Wrap popover content in a floating anchored layer attached to the trigger:
/// the caller `.child(anchored_menu(...))`s this from the trigger element while
/// open. Plays `menu-in` (0.14s fade + 2px drop); `closing` (the [`Popup`]
/// exit phase) swaps in `menu-out`. Dismissal is the caller's
/// `.on_mouse_down_out` on the content. The layer `.occlude()`s: hitboxes are
/// paint-order only in gpui, so without it clicks on menu rows would ALSO fire
/// whatever clickable sits under the floating layer.
pub fn anchored_menu(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    pinned_layer(
        gpui::deferred(
            gpui::anchored()
                .anchor(Anchor::TopLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(menu_motion(
                    id.into(),
                    exit,
                    div().occlude().pt(px(6.0)).child(content),
                )),
        )
        .priority(1)
        .into_any_element(),
    )
}

/// [`anchored_menu`] opening DOWNWARD from the trigger's bottom edge — a
/// dropdown proper (the sidebar's space filter). The default variant pins to
/// the trigger's top-left, which reads fine for context-style menus but
/// covers a button-shaped trigger.
pub fn anchored_menu_below(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    anchored_menu_below_gap(id, content, closing, 6.0)
}

/// [`anchored_menu_below`] right-aligned to the trigger's right edge. This is
/// the dropdown counterpart to [`anchored_menu_above_end`]: trailing sidebar
/// controls can open a full-width card leftward without leaving the sidebar.
pub fn anchored_menu_below_end(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    div()
        .absolute()
        .bottom_0()
        .right_0()
        .size_0()
        .child(
            gpui::deferred(
                gpui::anchored()
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu_motion(
                        id.into(),
                        exit,
                        div().occlude().pt(px(6.0)).child(content),
                    )),
            )
            .priority(1)
            .into_any_element(),
        )
        .into_any_element()
}

/// [`anchored_menu_below`] with a caller-chosen trigger→card gap — the
/// changes-header dropdowns hang off a tight titlebar band and need more
/// breathing room than the default 6px (user report; t3code sits near 10).
pub fn anchored_menu_below_gap(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
    gap: f32,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    div()
        .absolute()
        .bottom_0()
        .left_0()
        .size_0()
        .child(
            gpui::deferred(
                gpui::anchored()
                    .anchor(Anchor::TopLeft)
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu_motion(
                        id.into(),
                        exit,
                        div().occlude().pt(px(gap)).child(content),
                    )),
            )
            .priority(1)
            .into_any_element(),
        )
        .into_any_element()
}

/// [`anchored_menu`] opening UPWARD from the trigger (composer pickers, the
/// user menu — anything anchored near the window bottom; Radix flips these
/// automatically, gpui's `anchored` needs the side picked).
pub fn anchored_menu_above(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    pinned_layer(
        gpui::deferred(
            gpui::anchored()
                .anchor(Anchor::BottomLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(menu_motion(
                    id.into(),
                    exit,
                    div().occlude().pb(px(6.0)).child(content),
                )),
        )
        .priority(1)
        .into_any_element(),
    )
}

/// Open an upward menu at a point inside a relative trigger. Useful for text
/// completions, whose natural anchor is the token/caret rather than the input
/// element's outer edge.
pub fn anchored_menu_above_at(
    id: impl Into<SharedString>,
    position: Point<Pixels>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    div()
        .absolute()
        .left(position.x)
        .top(position.y)
        .size_0()
        .child(anchored_menu_above(id, content, closing))
        .into_any_element()
}

pub fn full_width_menu_above(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    div()
        .absolute()
        .bottom_full()
        .left_0()
        .right_0()
        .child(
            gpui::deferred(menu_motion(
                id.into(),
                exit,
                div().occlude().pb(px(6.0)).child(content),
            ))
            .priority(1),
        )
        .into_any_element()
}

/// [`anchored_menu_above`] right-aligned to the trigger's right edge (t3code
/// ComboboxPopup `align="end"` — right-side triggers like the composer's ref
/// picker open leftward instead of running off the window).
pub fn anchored_menu_above_end(
    id: impl Into<SharedString>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
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
                    .child(menu_motion(
                        id.into(),
                        exit,
                        div().occlude().pb(px(6.0)).child(content),
                    )),
            )
            .priority(1)
            .into_any_element(),
        )
        .into_any_element()
}

/// A floating menu at an explicit window position (context menus). Occludes
/// like [`anchored_menu`] so row clicks never reach elements underneath.
pub fn menu_at(
    id: impl Into<SharedString>,
    position: Point<Pixels>,
    content: AnyElement,
    closing: Option<std::time::Instant>,
) -> AnyElement {
    let exit = closing.map(exit_progress);
    let content = frosted_menu(exit, content);
    gpui::deferred(
        gpui::anchored()
            .position(position)
            .anchor(Anchor::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(menu_motion(id.into(), exit, div().occlude().child(content))),
    )
    .priority(1)
    .into_any_element()
}

/// Modal/overlay scrim at the *current* appearance, quoted in dark-mode terms
/// like [`ink`]/[`hairline`] — for callers (`modal`, the attachment lightbox)
/// that paint from a `deferred`/`anchored` layer with no `Theme`/`cx` in
/// scope. Mirrors [`Theme::scrim`], which is pinned at `X = 0.6` dark /
/// `0.32` light; other dark-mode alphas scale the light side by the same
/// ratio so the *dark* result is always exactly `alpha_dark` (never routed
/// through [`Hsla::opacity`], whose `0..=1` clamp would clip a
/// larger-than-0.6 alpha before it could scale the light side).
pub(crate) fn scrim_alpha(alpha_dark: f32) -> gpui::Hsla {
    crate::theme::scrim(alpha_dark)
}

/// Full-window modal: dim scrim + centered card with the `dialog-in` entrance.
/// The scrim swallows clicks; the caller wires its own dismiss/confirm.
/// `viewport` is the window size (an `anchored` layer sizes to its children,
/// so the scrim needs explicit dimensions). The frost radius matches
/// [`dialog_card`]'s 16px rounding.
pub fn modal(
    id: impl Into<ElementId>,
    viewport: gpui::Size<Pixels>,
    card: AnyElement,
) -> AnyElement {
    modal_with(id, viewport, card, 16.0, 0.6)
}

/// [`modal`] for glass-tinted cards (the add-space palette): a LIGHTER scrim,
/// so the frosted card reads like the popovers — the standard 0.6 dim buried
/// the backdrop hue under the blur and the palette came out a flat grey slab
/// next to the hue-inheriting menus (user report). `corner_radius` must match
/// the card's rounding.
pub fn modal_glass(
    id: impl Into<ElementId>,
    viewport: gpui::Size<Pixels>,
    card: AnyElement,
    corner_radius: f32,
) -> AnyElement {
    modal_with(id, viewport, card, corner_radius, 0.35)
}

fn modal_with(
    id: impl Into<ElementId>,
    viewport: gpui::Size<Pixels>,
    card: AnyElement,
    corner_radius: f32,
    scrim: f32,
) -> AnyElement {
    let card =
        crate::frost::frosted(corner_radius, crate::frost::MENU_BLUR, card).into_any_element();
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(scrim_alpha(scrim))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::dialog_in(id, div().child(card))),
            ),
    )
    .priority(2)
    .into_any_element()
}

/// One menu row (zeron `menuItem`): `gap-2.5 rounded-lg px-2 py-1.5
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
        .text_size(crate::typography::ui_rems(13.0))
        .cursor_pointer();
    if active {
        row.bg(crate::theme::card_selected_bg())
            .text_color(theme.text)
    } else {
        let fade_key = fade_key.into();
        let mut row = row
            .text_color(motion::hover_blend(
                &fade_key,
                theme.text.opacity(0.9),
                theme.text,
            ))
            .bg(motion::hover_blend(
                &fade_key,
                crate::theme::wash(0.0),
                crate::theme::card_selected_bg(),
            ));
        // Imperative form — the caller's `.id(...)` makes the element stateful
        // (hover listeners need element state, `.on_hover` needs `Stateful`).
        row.interactivity()
            .on_hover(motion::hover_listener(fade_key));
        row
    }
}

/// [`menu_row`] with a distinct keyboard-navigation highlight: a selected row
/// carries the full `bg-white/10` wash, the keyboard cursor the lighter
/// `bg-white/[0.08]` (zeron's `data-[highlighted]` styling) — two selected-
/// looking rows never appear at once.
pub fn menu_row_nav(
    theme: &Theme,
    selected: bool,
    highlighted: bool,
    fade_key: impl Into<SharedString>,
) -> gpui::Div {
    let row = menu_row(theme, selected, fade_key);
    if !selected && highlighted {
        row.bg(crate::theme::card_selected_bg())
            .text_color(theme.text)
    } else {
        row
    }
}

/// Small uppercase section heading inside a floating menu (zeron
/// `MenuHeading`): `px-2 pb-1 pt-1.5 text-[10px] font-medium uppercase
/// tracking-[0.1em] text-muted-foreground/60`. gpui has no letter-spacing at
/// the pinned rev; the tracking is approximated with hair spaces.
pub fn menu_heading(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(8.0))
        .pb(px(4.0))
        .pt(px(6.0))
        .text_size(crate::typography::ui_rems(10.0))
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

/// Hairline divider between menu sections (zeron `MenuSeparator`:
/// `mx-1 my-1 h-px bg-white/[0.07]`).
pub fn menu_separator() -> gpui::Div {
    // Full-bleed: negative margins cancel the card's p-1 inset so the hairline
    // runs border to border (user request).
    div().h(px(1.0)).mx(px(-4.0)).my(px(4.0)).bg(hairline(0.07))
}

/// The recessed band tone for a palette/picker header or footer strip — a
/// translucent black so the glass still reads through (the add-space palette
/// converged on this; measured subtler tones vanish against the dim scrim).
/// Free function (like [`ink`]/[`hairline`]/[`wash`]), mirroring
/// [`Theme::band`], for the several callers with no `Theme`/`cx` in scope
/// (some outside this crate's `ui` module tree — threading a `&Theme` param
/// would ripple past this task's file scope).
pub fn band() -> gpui::Hsla {
    crate::theme::band()
}

/// Shared shell for command-palette-style flows. The recessed header/footer
/// bands are supplied by callers, while this owns the glass tint, outline,
/// radius, clipping, and shadow that make Cmd+K and its sibling flows read as
/// one component family.
pub fn palette_card(theme: &Theme, width: Pixels, corner_radius: f32) -> gpui::Div {
    div()
        .w(width)
        .rounded(px(corner_radius))
        .border_1()
        .border_color(hairline(0.10))
        .bg(if theme.is_frost() {
            theme.glass_overlay()
        } else {
            theme.surface_overlay
        })
        .shadow_lg()
        .overflow_hidden()
        .flex()
        .flex_col()
        .text_color(theme.text)
}

/// One footer key-cap (22px, rounded-5, `white/[0.05]`) holding arbitrary
/// children — the base of [`key_hint`]/[`key_hint_pair`] and the search-bar
/// chips ("⌘K", "esc").
pub fn key_cap(_theme: &Theme) -> gpui::Div {
    div()
        .h(px(22.0))
        .px(px(5.0))
        .rounded(px(5.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .bg(ink(0.05))
}

/// The tiny verb after a key-cap.
fn key_hint_label(theme: &Theme, label: &'static str) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(10.5))
        .text_color(theme.text_muted.opacity(0.45))
        .child(SharedString::from(label))
}

/// A footer legend: one icon key-cap + tiny verb (the add-space palette's
/// footer voice, shared by the pickers).
pub fn key_hint(theme: &Theme, icon_path: &'static str, label: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .child(
            key_cap(theme).child(
                crate::icons::icon(icon_path)
                    .size(px(12.5))
                    .text_color(theme.text_muted.opacity(0.7)),
            ),
        )
        .child(key_hint_label(theme, label))
}

/// A footer legend whose cap holds a WORD ("tab", "esc") instead of a glyph
/// — for keys with no icon in the set.
pub fn key_hint_text(theme: &Theme, cap: &'static str, label: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .child(
            key_cap(theme)
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(cap)),
        )
        .child(key_hint_label(theme, label))
}

/// A footer legend whose cap holds TWO glyphs split by a hairline
/// ("[ ↑ | ↓ ] Navigate") sharing one verb.
pub fn key_hint_pair(
    theme: &Theme,
    first: &'static str,
    second: &'static str,
    label: &'static str,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .child(
            key_cap(theme)
                .child(
                    crate::icons::icon(first)
                        .size(px(12.5))
                        .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(div().w(px(1.0)).h(px(11.0)).bg(hairline(0.10)))
                .child(
                    crate::icons::icon(second)
                        .size(px(12.5))
                        .text_color(theme.text_muted.opacity(0.7)),
                ),
        )
        .child(key_hint_label(theme, label))
}

/// A muted kbd hint chip inside menu rows (`⌘↵`-style accelerators).
pub fn kbd_hint(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(5.0))
        .bg(ink(0.05))
        .text_size(crate::typography::ui_rems(10.0))
        .font_family(theme.font_mono.clone())
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from(label.to_string()))
}

/// The search/text input frame at the top of a picker popover (zeron
/// `searchInput`: `w-full rounded-lg bg-white/[0.04] px-2.5 py-1.5
/// text-[13px]` + `mb-1`, borderless — full width inside the card's own
/// p-1, only a 4px bottom margin).
pub fn search_input_frame(_theme: &Theme, input: AnyElement) -> gpui::Div {
    div()
        .mb(px(4.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(ink(0.04))
        .text_size(crate::typography::ui_rems(13.0))
        .child(input)
}

/// A bordered trailing menu section (zeron picker action groups /
/// branch-picker worktree block: `mt-1 flex flex-col gap-0.5 border-t
/// border-white/[0.06] pt-1` — the hairline runs edge-to-edge of the card's
/// p-1 inset, unlike [`menu_separator`]'s mx-1).
pub fn menu_section() -> gpui::Div {
    div()
        .mt(px(4.0))
        .pt(px(4.0))
        .border_t_1()
        .border_color(hairline(0.06))
        .flex()
        .flex_col()
        .gap(px(2.0))
}

// ---------------------------------------------------------------------------
// Dialog primitives (zeron dialog.tsx / sidebar dialogs.tsx)
// ---------------------------------------------------------------------------

/// The centered dialog card (`dialog-pop`): `w-[360px] rounded-2xl border
/// border-white/[0.1] bg-popover/95 p-5 shadow-2xl` — popover tone ≈ #101010.
pub fn dialog_card(theme: &Theme) -> gpui::Div {
    div()
        .w(px(360.0))
        .p(px(20.0))
        .rounded(px(16.0))
        .bg(theme.surface_dialog)
        .border_1()
        .border_color(hairline(0.10))
        .shadow_lg()
        .flex()
        .flex_col()
        .text_color(theme.text)
}

/// Dialog title: `text-[15px] font-semibold tracking-tight`.
pub fn dialog_title(theme: &Theme, title: &str) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(15.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(SharedString::from(title.to_string()))
}

/// Dialog body copy: `text-[13px] leading-relaxed text-muted-foreground`.
pub fn dialog_body(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(13.0))
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
        .border_color(hairline(0.08))
        .bg(ink(0.04))
        .text_size(crate::typography::ui_rems(14.0))
        .child(input)
}

/// Ghost button (`btnGhost`): quiet text, hover wash fading over
/// `transition-colors` (zeron dialogs.tsx). Caller adds id + click; `fade_key`
/// as in [`menu_row`].
pub fn btn_ghost(theme: &Theme, label: &str, fade_key: impl Into<SharedString>) -> gpui::Div {
    let fade_key = fade_key.into();
    let mut btn = div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(motion::hover_blend(&fade_key, theme.text_muted, theme.text))
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            ink(0.06),
        ))
        .cursor_pointer()
        .child(SharedString::from(label.to_string()));
    btn.interactivity()
        .on_hover(motion::hover_listener(fade_key));
    btn
}

/// Primary button (`btnPrimary`): white fill, near-black text.
pub fn btn_primary(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(theme.text)
        .text_size(crate::typography::ui_rems(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.on_solid)
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(SharedString::from(label.to_string()))
}

/// Destructive button (`btnDestructive`): the muted red fill.
pub fn btn_danger(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(theme.danger_strong)
        .text_size(crate::typography::ui_rems(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::white())
        .cursor_pointer()
        .hover(|s| s.opacity(0.9))
        .child(SharedString::from(label.to_string()))
}

/// Pulsing skeleton rows shown while a list loads (zeron:
/// `h-7 animate-pulse rounded-md bg-white/[0.04]`).
pub fn skeleton_rows(
    _id: &'static str,
    _theme: &Theme,
    count: usize,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    let wash = ink(0.04);
    let delta = motion::pulse_delta(&ZERON_PULSE, view, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .py(px(4.0))
        .children((0..count).map(move |i| {
            let phase = motion::staggered_phase(delta, i, 0.08);
            div()
                .h(px(28.0))
                .rounded(px(Theme::CONTROL_RADIUS))
                .bg(wash)
                .opacity(0.35 + 0.4 * motion::pulse_wave(phase))
        }))
        .into_any_element()
}

/// One pulsing ghost label — the trigger chip's label slot while the
/// selected model still resolves (a chip collapsing to its bare icon read
/// as broken; user report).
pub fn skeleton_bar(width: f32, view: gpui::EntityId, cx: &mut gpui::App) -> AnyElement {
    let delta = motion::pulse_delta(&ZERON_PULSE, view, cx);
    div()
        .w(px(width))
        .h(px(11.0))
        .rounded(px(5.5))
        .bg(ink(0.08))
        .opacity(0.35 + 0.4 * motion::pulse_wave(motion::staggered_phase(delta, 0, 0.0)))
        .into_any_element()
}

/// [`skeleton_rows`] shaped like a MENU loading: shorter bars of varied
/// widths reading as ghost labels rather than full-width slabs (the model
/// picker's loading state — reference design's skeleton). Widths cycle a
/// small deterministic ladder so the stagger reads organic without
/// randomness (randomness would repaint differently every open).
pub fn skeleton_menu_rows(
    _id: &'static str,
    _theme: &Theme,
    count: usize,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    const WIDTHS: [f32; 4] = [0.42, 0.58, 0.48, 0.66];
    let wash = ink(0.05);
    let delta = motion::pulse_delta(&ZERON_PULSE, view, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .py(px(6.0))
        .px(px(4.0))
        .children((0..count).map(move |i| {
            let phase = motion::staggered_phase(delta, i, 0.08);
            div()
                .h(px(14.0))
                .w(gpui::relative(WIDTHS[i % WIDTHS.len()]))
                .rounded(px(7.0))
                .bg(wash)
                .opacity(0.35 + 0.4 * motion::pulse_wave(phase))
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
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.danger)
        .child(gpui::SharedString::from(message.to_string()))
}

// ---------------------------------------------------------------------------
// Floating menu scrollbar — the model-list treatment, shared
// ---------------------------------------------------------------------------

/// Track inset top/bottom; the thumb travels inside it.
pub const MENU_SCROLLBAR_TRACK_INSET: f32 = 4.0;
/// Invisible hit strip width on the right edge.
pub const MENU_SCROLLBAR_HIT_WIDTH: f32 = 10.0;
/// Resting thumb width.
pub const MENU_SCROLLBAR_THUMB_WIDTH: f32 = 3.0;
/// Thumb width while hovered/dragged.
pub const MENU_SCROLLBAR_HOVER_THUMB_WIDTH: f32 = 5.0;
/// Smallest readable thumb on very long lists.
pub const MENU_SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Geometry of the floating thumb for a scroll viewport at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuScrollbarMetrics {
    pub track_height: f32,
    pub thumb_top: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

impl MenuScrollbarMetrics {
    /// Distance the thumb itself can travel.
    pub fn travel(self) -> f32 {
        (self.track_height - self.thumb_height).max(0.0)
    }

    /// Pure geometry from the viewport and scroll distances. `None` when the
    /// content fits (`max_scroll <= 0`) or the viewport is too small to hold
    /// a track.
    pub fn from_viewport(
        viewport_height: f32,
        max_scroll: f32,
        current_scroll: f32,
    ) -> Option<Self> {
        let max_scroll = max_scroll.max(0.0);
        if viewport_height <= 0.0 || max_scroll <= 0.0 {
            return None;
        }
        let track_height = (viewport_height - MENU_SCROLLBAR_TRACK_INSET * 2.0).max(0.0);
        if track_height <= 0.0 {
            return None;
        }
        let content_height = viewport_height + max_scroll;
        let thumb_height = (track_height * viewport_height / content_height)
            .max(MENU_SCROLLBAR_MIN_THUMB)
            .min(track_height);
        let current_scroll = current_scroll.clamp(0.0, max_scroll);
        let travel = (track_height - thumb_height).max(0.0);
        Some(Self {
            track_height,
            thumb_top: travel * current_scroll / max_scroll,
            thumb_height,
            max_scroll,
        })
    }
}

/// Marker for GPUI's captured drag stream. The actual grab geometry stays in
/// [`MenuScrollbarState`] so a track click can center the thumb first.
pub struct MenuScrollbarDrag;

/// Invisible drag preview: scrollbar drags manipulate the existing thumb.
pub struct MenuScrollbarDragGhost;

impl gpui::Render for MenuScrollbarDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

/// Hover/drag interaction state for one floating scrollbar, owned by the view
/// that renders the list. Event handlers stay on the view (they need its
/// listeners) and delegate here; only one list surface owns a state at a
/// time, so mutually exclusive popups may share one instance.
#[derive(Default)]
pub struct MenuScrollbarState {
    list_hovered: bool,
    bar_hovered: bool,
    grab: Option<f32>,
}

impl MenuScrollbarState {
    /// Metrics from any scroll handle's live bounds/offset — both
    /// `ScrollHandle` and a virtualized list's base handle qualify. `None`
    /// when the content fits.
    pub fn metrics(&self, scroll: &gpui::ScrollHandle) -> Option<MenuScrollbarMetrics> {
        let bounds = scroll.bounds();
        // GPUI stores the maximum as a positive distance; only the live
        // scroll offset is negative while content moves upward.
        let max_scroll = f32::from(scroll.max_offset().y).max(0.0);
        let current_scroll = (-f32::from(scroll.offset().y)).clamp(0.0, max_scroll);
        MenuScrollbarMetrics::from_viewport(
            f32::from(bounds.size.height),
            max_scroll,
            current_scroll,
        )
    }

    /// Whether the rail paints at all — an on-demand affordance like the
    /// model list's: hidden until the list is hovered or a drag holds it.
    pub fn visible(&self) -> bool {
        self.list_hovered || self.grab.is_some()
    }

    /// Whether the thumb carries the expanded/stronger treatment.
    pub fn active(&self) -> bool {
        self.bar_hovered || self.grab.is_some()
    }

    /// The pointer entered/left the LIST. Returns whether anything changed.
    pub fn set_list_hovered(&mut self, hovered: bool) -> bool {
        if self.list_hovered == hovered {
            return false;
        }
        self.list_hovered = hovered;
        if !hovered && self.grab.is_none() {
            self.bar_hovered = false;
        }
        true
    }

    /// The pointer entered/left the RAIL. Keeps the active treatment while a
    /// captured drag travels outside (the hover callback correctly turns
    /// false there). Returns whether anything changed.
    pub fn set_bar_hovered(&mut self, hovered: bool) -> bool {
        let active = hovered || self.grab.is_some();
        if self.bar_hovered == active {
            return false;
        }
        self.bar_hovered = active;
        true
    }

    /// A press landed on the rail: choose the grab point (pressing the thumb
    /// keeps its relative position; pressing the track centers the thumb
    /// under the pointer first), engage the drag, and scroll to the pointer.
    /// `false` when there is nothing to scroll.
    pub fn begin_press(&mut self, scroll: &gpui::ScrollHandle, pointer_y: Pixels) -> bool {
        let Some(metrics) = self.metrics(scroll) else {
            return false;
        };
        let pointer_in_track = self.pointer_in_track(scroll, pointer_y);
        let grab_offset = if (metrics.thumb_top..=metrics.thumb_top + metrics.thumb_height)
            .contains(&pointer_in_track)
        {
            pointer_in_track - metrics.thumb_top
        } else {
            metrics.thumb_height / 2.0
        };
        self.grab = Some(grab_offset);
        self.drag_to(scroll, pointer_y);
        true
    }

    /// Move an engaged drag to `pointer_y`. `false` when no drag is engaged
    /// or the content stopped scrolling mid-drag.
    pub fn drag_to(&self, scroll: &gpui::ScrollHandle, pointer_y: Pixels) -> bool {
        let Some(grab_offset) = self.grab else {
            return false;
        };
        let Some(metrics) = self.metrics(scroll) else {
            return false;
        };
        let thumb_top =
            (self.pointer_in_track(scroll, pointer_y) - grab_offset).clamp(0.0, metrics.travel());
        let scroll_to = if metrics.travel() <= 0.0 {
            0.0
        } else {
            thumb_top / metrics.travel() * metrics.max_scroll
        };
        let offset = scroll.offset();
        scroll.set_offset(gpui::Point::new(offset.x, px(-scroll_to)));
        true
    }

    /// The press ended anywhere: drop the drag; the rail stays armed only
    /// while the list is still hovered. Returns whether anything changed.
    pub fn end_press(&mut self) -> bool {
        self.grab = None;
        if !self.list_hovered && self.bar_hovered {
            self.bar_hovered = false;
            return true;
        }
        false
    }

    fn pointer_in_track(&self, scroll: &gpui::ScrollHandle, pointer_y: Pixels) -> f32 {
        f32::from(pointer_y - scroll.bounds().top()) - MENU_SCROLLBAR_TRACK_INSET
    }

    /// The positioned rail visuals: a full-height hit strip on the right with
    /// the thumb inside. Callers layer identity + their own listeners onto
    /// the returned strip — `.id(...)` first (hover needs element state),
    /// then `.on_hover`, `.on_mouse_down`,
    /// `.on_drag(MenuScrollbarDrag, |_, _, _, cx| { cx.stop_propagation();
    /// cx.new(|_| MenuScrollbarDragGhost) })`, `.on_mouse_up_out` /
    /// `.on_mouse_up`. `None` while hidden or the content fits.
    pub fn render_rail(&self, theme: &Theme, metrics: MenuScrollbarMetrics) -> Option<Div> {
        if !self.visible() {
            return None;
        }
        let active = self.active();
        let thumb_width = if active {
            MENU_SCROLLBAR_HOVER_THUMB_WIDTH
        } else {
            MENU_SCROLLBAR_THUMB_WIDTH
        };
        Some(
            div()
                .absolute()
                .top(px(0.0))
                .bottom(px(0.0))
                .right(px(0.0))
                .w(px(MENU_SCROLLBAR_HIT_WIDTH))
                // The thumb is an absolute child inside a fixed-width hit
                // rail, so hover expansion never reflows rows.
                .child(
                    div()
                        .absolute()
                        .top(px(MENU_SCROLLBAR_TRACK_INSET + metrics.thumb_top))
                        .right(px(2.0))
                        .w(px(thumb_width))
                        .h(px(metrics.thumb_height))
                        .rounded(px(thumb_width / 2.0))
                        .bg(theme.text_faint.opacity(if active { 0.68 } else { 0.5 })),
                ),
        )
    }
}

// ---------------------------------------------------------------------------
// Horizontal floating scrollbar — the same quiet rail used by menus, rotated
// for local code planes. Kept separate from `MenuScrollbarState` so a code
// fence can own one state per stable block while existing menu callers retain
// their vertical API.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizontalScrollbarMetrics {
    pub track_width: f32,
    pub thumb_left: f32,
    pub thumb_width: f32,
    pub max_scroll: f32,
}

impl HorizontalScrollbarMetrics {
    pub fn travel(self) -> f32 {
        (self.track_width - self.thumb_width).max(0.0)
    }

    pub fn from_viewport(
        viewport_width: f32,
        max_scroll: f32,
        current_scroll: f32,
    ) -> Option<Self> {
        let max_scroll = max_scroll.max(0.0);
        if viewport_width <= 0.0 || max_scroll <= 0.0 {
            return None;
        }
        let track_width = (viewport_width - MENU_SCROLLBAR_TRACK_INSET * 2.0).max(0.0);
        if track_width <= 0.0 {
            return None;
        }
        let content_width = viewport_width + max_scroll;
        let thumb_width = (track_width * viewport_width / content_width)
            .max(MENU_SCROLLBAR_MIN_THUMB)
            .min(track_width);
        let current_scroll = current_scroll.clamp(0.0, max_scroll);
        let travel = (track_width - thumb_width).max(0.0);
        Some(Self {
            track_width,
            thumb_left: travel * current_scroll / max_scroll,
            thumb_width,
            max_scroll,
        })
    }
}

/// Hover/drag state for one horizontal code viewport. Geometry comes from the
/// same tracked [`gpui::ScrollHandle`] that moves the code, so the thumb always
/// represents the block's real local overflow (virtual transcript height is
/// irrelevant here).
#[derive(Default)]
pub struct HorizontalScrollbarState {
    viewport_hovered: bool,
    bar_hovered: bool,
    grab: Option<f32>,
}

impl HorizontalScrollbarState {
    pub fn metrics(&self, scroll: &gpui::ScrollHandle) -> Option<HorizontalScrollbarMetrics> {
        let bounds = scroll.bounds();
        let max_scroll = f32::from(scroll.max_offset().x).max(0.0);
        let current_scroll = (-f32::from(scroll.offset().x)).clamp(0.0, max_scroll);
        HorizontalScrollbarMetrics::from_viewport(
            f32::from(bounds.size.width),
            max_scroll,
            current_scroll,
        )
    }

    pub fn visible(&self) -> bool {
        self.viewport_hovered || self.grab.is_some()
    }

    pub fn active(&self) -> bool {
        self.bar_hovered || self.grab.is_some()
    }

    pub fn set_viewport_hovered(&mut self, hovered: bool) -> bool {
        if self.viewport_hovered == hovered {
            return false;
        }
        self.viewport_hovered = hovered;
        if !hovered && self.grab.is_none() {
            self.bar_hovered = false;
        }
        true
    }

    pub fn set_bar_hovered(&mut self, hovered: bool) -> bool {
        let active = hovered || self.grab.is_some();
        if self.bar_hovered == active {
            return false;
        }
        self.bar_hovered = active;
        true
    }

    pub fn begin_press(&mut self, scroll: &gpui::ScrollHandle, pointer_x: Pixels) -> bool {
        let Some(metrics) = self.metrics(scroll) else {
            return false;
        };
        let pointer_in_track = self.pointer_in_track(scroll, pointer_x);
        let grab_offset = if (metrics.thumb_left..=metrics.thumb_left + metrics.thumb_width)
            .contains(&pointer_in_track)
        {
            pointer_in_track - metrics.thumb_left
        } else {
            metrics.thumb_width / 2.0
        };
        self.grab = Some(grab_offset);
        self.drag_to(scroll, pointer_x);
        true
    }

    pub fn drag_to(&self, scroll: &gpui::ScrollHandle, pointer_x: Pixels) -> bool {
        let Some(grab_offset) = self.grab else {
            return false;
        };
        let Some(metrics) = self.metrics(scroll) else {
            return false;
        };
        let thumb_left =
            (self.pointer_in_track(scroll, pointer_x) - grab_offset).clamp(0.0, metrics.travel());
        let scroll_to = if metrics.travel() <= 0.0 {
            0.0
        } else {
            thumb_left / metrics.travel() * metrics.max_scroll
        };
        let offset = scroll.offset();
        scroll.set_offset(gpui::Point::new(px(-scroll_to), offset.y));
        true
    }

    pub fn end_press(&mut self) -> bool {
        self.grab = None;
        if !self.viewport_hovered && self.bar_hovered {
            self.bar_hovered = false;
            return true;
        }
        false
    }

    fn pointer_in_track(&self, scroll: &gpui::ScrollHandle, pointer_x: Pixels) -> f32 {
        f32::from(pointer_x - scroll.bounds().left()) - MENU_SCROLLBAR_TRACK_INSET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_press_note_distinguishes_dismiss_from_open() {
        let mut popup: Popup<u8> = Popup::default();

        // Fresh open: press finds nothing mounted → click opens.
        popup.note_trigger_press();
        assert!(!popup.take_press_was_open());
        popup.open(1);

        // Trigger click while open: the card's mouse-down-out begins the
        // close on the press (either handler order) — the note still reads
        // mounted, so the click must NOT reopen.
        popup.note_trigger_press();
        popup.begin_close();
        assert!(popup.take_press_was_open());
        // Out-handler first, trigger note second: mid-exit still counts.
        popup.open(1);
        popup.begin_close();
        popup.note_trigger_press();
        assert!(popup.take_press_was_open());

        // The note is consumed — a later click starts clean.
        assert!(!popup.take_press_was_open());

        // Kind-keyed popups: a press on a DIFFERENT trigger doesn't count,
        // so that click switches menus instead of swallowing.
        let mut popup: Popup<u8> = Popup::default();
        popup.open(1);
        popup.note_trigger_press_matching(|kind| *kind == 2);
        assert!(!popup.take_press_was_open());
        popup.note_trigger_press_matching(|kind| *kind == 1);
        assert!(popup.take_press_was_open());
    }

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
        // Readline motion — only with ctrl held.
        assert_eq!(classify_key("n", false, true), MenuKey::Down);
        assert_eq!(classify_key("p", false, true), MenuKey::Up);
        assert_eq!(classify_key("n", false, false), MenuKey::Other);
        assert_eq!(classify_key("p", true, false), MenuKey::Other);
    }

    #[test]
    fn tracked_upper_spaces_letters() {
        assert_eq!(tracked_upper("ab"), "A\u{200A}B");
        assert_eq!(
            tracked_upper("Question"),
            "Q\u{200A}U\u{200A}E\u{200A}S\u{200A}T\u{200A}I\u{200A}O\u{200A}N"
        );
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

    #[test]
    fn scrollbar_metrics_hidden_when_content_fits_or_viewport_tiny() {
        // No overflow → no scrollbar.
        assert_eq!(MenuScrollbarMetrics::from_viewport(300.0, 0.0, 0.0), None);
        assert_eq!(MenuScrollbarMetrics::from_viewport(300.0, -5.0, 0.0), None);
        // No viewport → no scrollbar.
        assert_eq!(MenuScrollbarMetrics::from_viewport(0.0, 300.0, 0.0), None);
        // Viewport smaller than two track insets → no track.
        assert_eq!(MenuScrollbarMetrics::from_viewport(8.0, 300.0, 0.0), None);
    }

    #[test]
    fn scrollbar_metrics_scales_thumb_to_content_ratio() {
        let m = MenuScrollbarMetrics::from_viewport(300.0, 300.0, 150.0).unwrap();
        // Track = 300 - 2*4; thumb = half the content (600) → 146.
        assert_eq!(m.track_height, 292.0);
        assert_eq!(m.thumb_height, 146.0);
        assert_eq!(m.travel(), 146.0);
        // Half-scrolled puts the thumb mid-track.
        assert_eq!(m.thumb_top, 73.0);
        assert_eq!(m.max_scroll, 300.0);
    }

    #[test]
    fn scrollbar_metrics_clamps_min_thumb_and_position() {
        let m = MenuScrollbarMetrics::from_viewport(100.0, 9900.0, 4950.0).unwrap();
        // Raw ratio (92 * 100 / 10000 ≈ 0.92px) clamps to the readable minimum.
        assert_eq!(m.thumb_height, MENU_SCROLLBAR_MIN_THUMB);
        assert_eq!(m.travel(), 92.0 - MENU_SCROLLBAR_MIN_THUMB);
        assert_eq!(m.thumb_top, (92.0 - MENU_SCROLLBAR_MIN_THUMB) / 2.0);
        // Overscroll clamps to the bottom of the track.
        let m = MenuScrollbarMetrics::from_viewport(100.0, 9900.0, 99_999.0).unwrap();
        assert_eq!(m.thumb_top, 92.0 - MENU_SCROLLBAR_MIN_THUMB);
        // Negative offsets clamp to the top.
        let m = MenuScrollbarMetrics::from_viewport(100.0, 9900.0, -3.0).unwrap();
        assert_eq!(m.thumb_top, 0.0);
    }

    #[test]
    fn horizontal_scrollbar_metrics_match_the_vertical_treatment() {
        let m = HorizontalScrollbarMetrics::from_viewport(300.0, 300.0, 150.0).unwrap();
        assert_eq!(m.track_width, 292.0);
        assert_eq!(m.thumb_width, 146.0);
        assert_eq!(m.travel(), 146.0);
        assert_eq!(m.thumb_left, 73.0);
        assert_eq!(m.max_scroll, 300.0);
    }

    #[test]
    fn horizontal_scrollbar_hides_without_overflow_and_clamps_position() {
        assert_eq!(
            HorizontalScrollbarMetrics::from_viewport(300.0, 0.0, 0.0),
            None
        );
        let m = HorizontalScrollbarMetrics::from_viewport(100.0, 9900.0, 99_999.0).unwrap();
        assert_eq!(m.thumb_left, 92.0 - MENU_SCROLLBAR_MIN_THUMB);
        let m = HorizontalScrollbarMetrics::from_viewport(100.0, 9900.0, -3.0).unwrap();
        assert_eq!(m.thumb_left, 0.0);
    }
}
