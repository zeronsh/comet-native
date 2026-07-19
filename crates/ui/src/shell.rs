//! The app shell (comet `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is comet's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360–760px, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Empty, Entity, IntoElement, MouseButton, MouseUpEvent, Point,
    Render, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::loaders;
use crate::motion::{self, AnimationExt as _, RESIZE, SPLASH_OUT};
use crate::settings::{
    RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS, SIDEBAR_DEFAULT,
    SIDEBAR_MAX, SIDEBAR_MIN, UiSettings,
};
use crate::state::{AppState, ConnectionStatus, EngineBootConfig, EngineMode, GatePhase, Indicator};
use crate::theme::Theme;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out). `epoch` keys the animation element so
/// each toggle restarts the timeline.
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    epoch: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

pub struct Shell {
    state: Entity<AppState>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    tween_epoch: usize,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    _state_observation: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        Self {
            state,
            boot,
            data_dir,
            settings,
            sidebar_tween: None,
            right_tween: None,
            tween_epoch: 0,
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            _state_observation: observation,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed { 0.0 } else { self.settings.sidebar_width }
    }

    fn right_target(&self) -> f32 {
        if self.settings.right_pane_open { self.settings.right_pane_width } else { 0.0 }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.tween_epoch += 1;
        self.sidebar_tween =
            Some(WidthTween { from, to: self.sidebar_target(), epoch: self.tween_epoch });
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target();
        self.settings.right_pane_open = !self.settings.right_pane_open;
        self.tween_epoch += 1;
        self.right_tween =
            Some(WidthTween { from, to: self.right_target(), epoch: self.tween_epoch });
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // comet caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(SAVE_DEBOUNCE_MS)).await;
            let Ok(snapshot) = this.update(cx, |shell, _| shell.settings.clone()) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- render pieces ----

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        id_base: &'static str,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        let container = div().h_full().flex_none().overflow_hidden().child(inner);
        match tween {
            Some(WidthTween { from, to, epoch }) => container
                .with_animation((id_base, epoch), RESIZE.animation(), move |el, t| {
                    el.w(px(motion::lerp(from, to, t)))
                })
                .into_any_element(),
            None => container.w(px(target)).into_any_element(),
        }
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let (bg, border, hover, active, text, muted, faint, accent, warning, danger) = (
            theme.surface,
            theme.border,
            theme.element_hover,
            theme.element_active,
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.accent,
            theme.warning,
            theme.danger,
        );

        // Snapshot row data first; listeners capture ids, not borrows.
        let now = Utc::now();
        let (rows, user_line) = {
            let state = self.state.read(cx);
            let rows: Vec<(String, SharedString, Indicator, bool)> = state
                .visible_chats()
                .map(|chat| {
                    (
                        chat.id.clone(),
                        chat.title.clone().unwrap_or_else(|| "New session".into()).into(),
                        state.indicator_for(&chat.id, now),
                        state.selected_chat.as_deref() == Some(chat.id.as_str()),
                    )
                })
                .collect();
            let user_line: SharedString = match &state.auth {
                Some(comet_proto::AuthState::SignedIn { user, .. }) => user.email.clone().into(),
                _ => "Not signed in".into(),
            };
            (rows, user_line)
        };

        let inner = div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // Device switcher placeholder (real switcher lands with WatchDevices data).
            .child(
                div()
                    .h(px(Theme::HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_MD))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(muted)
                            .child(SharedString::from("This device")),
                    ),
            )
            // "New session" button (wired to CreateChat in M4's Mutate surface).
            .child(
                div()
                    .id("new-session")
                    .mx(px(Theme::SPACE_MD))
                    .mb(px(Theme::SPACE_SM))
                    .px(px(Theme::SPACE_MD))
                    .py(px(6.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(border)
                    .text_size(px(13.0))
                    .text_color(text)
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Placeholder: clears selection back to the new-session canvas.
                        this.state.update(cx, |s, cx| s.select_chat(None, cx));
                    }))
                    .child(SharedString::from("New session")),
            )
            // Chat list.
            .child(
                div()
                    .id("chat-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows.into_iter().map(|(id, title, indicator, selected)| {
                        let dot_color = match indicator {
                            Indicator::Working => Some(accent),
                            Indicator::AwaitingInput => Some(warning),
                            Indicator::Errored => Some(danger),
                            Indicator::None => None,
                        };
                        let select_id = id.clone();
                        div()
                            .id(SharedString::from(id))
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .px(px(Theme::SPACE_SM))
                            .py(px(5.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(13.0))
                            .text_color(if selected { text } else { muted })
                            .when(selected, |el| el.bg(active))
                            .hover(move |s| s.bg(hover))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = select_id.clone();
                                this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
                            }))
                            .when_some(dot_color, |el, color| {
                                el.child(div().size(px(6.0)).rounded_full().flex_none().bg(color))
                            })
                            .child(div().flex_1().truncate().child(title))
                    })),
            )
            // UserMenu placeholder.
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(border)
                    .px(px(Theme::SPACE_MD))
                    .py(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(faint)
                    .child(user_line),
            );

        let target = self.sidebar_target();
        self.pane_container(
            "sidebar-width",
            self.sidebar_tween,
            target,
            div().h_full().bg(bg).when(target > 0.0, |el| el.border_r_1().border_color(border))
                .child(inner)
                .into_any_element(),
        )
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let (border, text, muted, faint, hover) = (
            theme.border,
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.element_hover,
        );
        let (title, transcript_len, mode_line) = {
            let state = self.state.read(cx);
            let title: SharedString = state
                .selected_chat_row()
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "comet".into())
                .into();
            let mode_line: SharedString = match state.engine().map(|e| e.mode()) {
                Some(EngineMode::InProcess) => "engine: in-process".into(),
                Some(EngineMode::Remote { url }) => format!("engine: {url}").into(),
                None => "".into(),
            };
            (title, state.transcript.len(), mode_line)
        };
        let has_selection = self.state.read(cx).selected_chat.is_some();

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            // Header (h-11): collapse toggle + title + right-pane toggle.
            .child(
                div()
                    .h(px(Theme::HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_MD))
                    .px(px(Theme::SPACE_MD))
                    .border_b_1()
                    .border_color(border)
                    .child(header_button("toggle-sidebar", "☰", hover, muted, cx.listener(
                        |this, _, _, cx| this.toggle_sidebar(cx),
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(text)
                            .child(title),
                    )
                    .child(header_button("toggle-changes", "Changes", hover, muted, cx.listener(
                        |this, _, _, cx| this.toggle_right_pane(cx),
                    ))),
            )
            // Content outlet — conversation view arrives in M3b.
            .child(
                div().flex_1().min_h_0().flex().items_center().justify_center().child(
                    motion::fade_in(
                        "outlet-placeholder",
                        div().text_size(px(13.0)).text_color(faint).child(SharedString::from(
                            if has_selection {
                                format!("{transcript_len} transcript entries")
                            } else {
                                "Send a message to start".to_string()
                            },
                        )),
                    ),
                ),
            )
            // Reserved status strip (h-6) — WorkingIndicator slot.
            .child(
                div()
                    .h(px(Theme::STATUS_STRIP_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_MD))
                    .text_size(px(11.0))
                    .text_color(faint)
                    .child(mode_line),
            )
            .into_any_element()
    }

    /// Right "Changes" pane scaffold — hidden by default, drag-resizable.
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let (bg, border, faint) = (theme.surface, theme.border, theme.text_faint);
        let inner = div()
            .w(px(self.settings.right_pane_width))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(faint)
            .child(SharedString::from("Changes pane"));
        let target = self.right_target();
        self.pane_container(
            "right-pane-width",
            self.right_tween,
            target,
            div().h_full().bg(bg).when(target > 0.0, |el| el.border_l_1().border_color(border))
                .child(inner)
                .into_any_element(),
        )
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let (raised, border, text, muted, danger, hover) = (
            theme.surface_raised,
            theme.border,
            theme.text,
            theme.text_muted,
            theme.danger,
            theme.element_hover,
        );
        let card = div()
            .w(px(360.0))
            .p(px(Theme::SPACE_LG))
            .rounded(px(Theme::PANEL_RADIUS))
            .bg(raised)
            .border_1()
            .border_color(border)
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .text_size(px(13.0));
        let card = match phase {
            GatePhase::Failed(error) => card
                .child(div().text_color(danger).child(SharedString::from("Backend unreachable")))
                .child(div().text_color(muted).child(SharedString::from(error.clone())))
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(Theme::SPACE_MD))
                        .py(px(6.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(border)
                        .text_color(text)
                        .hover(move |s| s.bg(hover))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                ),
            // Sign-in card placeholder — the WorkOS flow lands with M4 auth.
            _ => card
                .child(div().text_color(text).child(SharedString::from("Sign in")))
                .child(div().text_color(muted).child(SharedString::from(
                    "Authentication arrives with M4 — run in dev mode for now.",
                ))),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(motion::dialog_in("gate-card", card))
            .into_any_element()
    }
}

fn header_button(
    id: &'static str,
    label: &'static str,
    hover: gpui::Hsla,
    color: gpui::Hsla,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(Theme::SPACE_SM))
        .py(px(3.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .text_size(px(12.0))
        .text_color(color)
        .hover(move |s| s.bg(hover))
        .cursor_pointer()
        .on_click(on_click)
        .child(SharedString::from(label))
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let (bg, text, font) = (theme.bg, theme.text, theme.font_sans.clone());
        let gate = self.state.read(cx).gate();

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(bg)
            .text_color(text)
            .font_family(font)
            .text_size(px(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag));

        let root = match &gate {
            GatePhase::Ready => {
                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                let right_open = self.settings.right_pane_open;
                let right_handle = self.resize_handle(
                    "right-pane-resize",
                    || RightPaneResize,
                    |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                    cx,
                );
                let right = self.render_right_pane(cx);
                root.child(sidebar)
                    .child(sidebar_handle)
                    .child(main)
                    .when(right_open, |el| el.child(right_handle))
                    .child(right)
            }
            GatePhase::Loading => root, // splash overlay covers boot
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        match self.splash {
            SplashPhase::Visible => root.child(loaders::splash_overlay(Theme::of(cx), false)),
            SplashPhase::FadingOut => root.child(loaders::splash_overlay(Theme::of(cx), true)),
            SplashPhase::Gone => root,
        }
    }
}
