//! The session tab strip — replaces the chat header (feature spec: spaces
//! overhaul). Every non-archived session of the selected space is a tab:
//! agent brand icon + title + a trailing slot that shows the status dot at
//! rest and swaps to a close button on hover. `+` at the end opens the
//! new-session canvas (the tab materializes on first send). The strip inherits
//! the old header's titlebar duties: 44px tall, drag region, animated
//! window-controls inset, and the toggle-changes button (git spaces only).
//!
//! Styling and drag-reorder mirror the terminal tab bar
//! (`terminal/panel.rs::render_tab_bar`) — same fixed-width tabs, drop-index
//! math, 150ms sibling slide, and drag ghost. The manual order is device-local
//! (`UiSettings.tab_order`, keyed by space). Overflow scrolls horizontally
//! with edge fades.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use comet_proto::ChatIndicator;

/// Fixed tab width (terminal tabs use 118; session titles get a bit more).
pub(super) const SESSION_TAB_WIDTH: f32 = 140.0;
/// Flex gap between tabs — part of the drop-index slot width.
const TAB_GAP: f32 = 4.0;
/// Width of the overflow edge fades.
const FADE_WIDTH: f32 = 24.0;

/// Drag-reorder state; `epoch` keys the 150ms slide animation restarts.
pub(super) struct TabDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-tab payload (gpui drag-and-drop), space-scoped.
struct TabDragPayload {
    space: String,
    from: usize,
    title: SharedString,
    brand: Option<(&'static str, Option<gpui::Hsla>)>,
}

/// The floating tab rendered at the cursor while dragging.
struct TabGhost {
    title: SharedString,
    brand: Option<(&'static str, Option<gpui::Hsla>)>,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(SESSION_TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.0))
            .text_color(theme.text)
            .opacity(0.85)
            .when_some(self.brand, |el, (path, tint)| {
                el.child(
                    icon(path)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
            })
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Resolve the visual tab order for a space: the manual (drag) order first —
/// skipping chats that no longer exist — then any new chats appended in
/// creation order. Pure.
pub(super) fn resolve_tab_order(created_order: &[String], manual: &[String]) -> Vec<String> {
    let mut out: Vec<String> = manual
        .iter()
        .filter(|id| created_order.contains(id))
        .cloned()
        .collect();
    for id in created_order {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// The neighbor to select after closing `closed`: the next tab, else the
/// previous, else `None` (last tab → new-session canvas). Pure.
pub(super) fn next_after_close(order: &[String], closed: &str) -> Option<String> {
    let ix = order.iter().position(|id| id == closed)?;
    if order.len() <= 1 {
        return None;
    }
    Some(if ix + 1 < order.len() {
        order[ix + 1].clone()
    } else {
        order[ix - 1].clone()
    })
}

impl Shell {
    /// The space's tabs in VISUAL order (manual drag order over creation order).
    fn tab_ids(&self, space_id: &str, cx: &App) -> Vec<String> {
        let created: Vec<String> = self
            .state
            .read(cx)
            .chats_in_space(space_id)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        match self.settings.tab_order.get(space_id) {
            Some(manual) => resolve_tab_order(&created, manual),
            None => created,
        }
    }

    /// Close a tab = archive the session. Selection moves to a neighbor; the
    /// last tab lands on the new-session canvas.
    pub(super) fn close_session_tab(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let (selected, order) = {
            let space = self.state.read(cx).selected_space.clone();
            let order = space
                .as_deref()
                .map(|space| self.tab_ids(space, cx))
                .unwrap_or_default();
            (self.state.read(cx).selected_chat.clone(), order)
        };
        if selected.as_deref() == Some(chat_id.as_str()) {
            let next = next_after_close(&order, &chat_id);
            self.state.update(cx, |s, cx| s.select_chat(next, cx));
        }
        self.archive_chat(chat_id, cx);
    }

    /// Track the drop slot while a tab is dragged over the strip (150ms sibling
    /// slides restart per committed `over` change — terminal-panel idiom).
    fn update_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.tab_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.tab_drag = Some(TabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order for the space (device-local).
    fn commit_tab_reorder(&mut self, space: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        let mut order = self.tab_ids(space, cx);
        if from < order.len() {
            reorder_tabs(&mut order, from, to);
            self.settings.tab_order.insert(space.to_string(), order);
            self.schedule_save(cx);
        }
        self.tab_drag = None;
        cx.notify();
    }

    /// The tab strip: [scrollable tabs (edge fades)][+][drag spacer][toggle-changes].
    pub(super) fn render_session_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        // A drag that ended off-strip (no drop event) must not strand the
        // sibling slide offsets.
        if self.tab_drag.is_some() && !cx.has_active_drag() {
            self.tab_drag = None;
        }
        let space_id = self.state.read(cx).selected_space.clone();
        let order: Vec<String> = space_id
            .as_deref()
            .map(|space| self.tab_ids(space, cx))
            .unwrap_or_default();
        let tabs: Vec<(String, SharedString, Option<comet_proto::HarnessId>, ChatIndicator)> = {
            let state = self.state.read(cx);
            order
                .iter()
                .filter_map(|id| {
                    let chat = state.chats.iter().find(|c| c.id == *id)?;
                    Some((
                        chat.id.clone(),
                        SharedString::from(transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )),
                        chat.config.as_ref().map(|c| c.harness),
                        state.display_status_for(chat, now),
                    ))
                })
                .collect()
        };
        let selected = self.state.read(cx).selected_chat.clone();
        let has_space = space_id.is_some();
        let git = self.space_git_detected(cx);
        let hovered = self.tab_hover.clone();
        let on_canvas = selected.is_none();
        // No sessions yet → the canvas already shows; a `+` would be redundant.
        let has_tabs = !tabs.is_empty();
        let count = tabs.len();
        let drag = self
            .tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        let tab_elements: Vec<AnyElement> = tabs
            .into_iter()
            .enumerate()
            .map(|(ix, (id, title, harness, status))| {
                let is_selected = selected.as_deref() == Some(id.as_str());
                let is_hovered = hovered.as_deref() == Some(id.as_str());
                // Hover state lives in Shell (the trailing slot swaps dot ↔
                // close), so the wash snaps off it too — gpui allows only one
                // `on_hover` per element, and the state listener wins.
                let (text_color, bg) = if is_selected {
                    (theme.text, crate::theme::white_alpha(0.08))
                } else if is_hovered {
                    (theme.text_muted.opacity(0.8), theme.element_hover)
                } else {
                    (theme.text_muted.opacity(0.6), gpui::transparent_black())
                };
                let glyph_alpha = if is_selected { 0.9 } else { 0.6 };
                let brand = harness.map(crate::pickers::harness_brand_icon);
                let select_id = id.clone();
                let close_id = id.clone();
                let middle_id = id.clone();
                let hover_id = id.clone();
                let drag_space = space_id.clone().unwrap_or_default();
                // NB: no `.occlude()` on the close button — the TAB already
                // occludes (for the titlebar drag region), and an occluding
                // child would block the tab's own hover hit-test: a flicker
                // loop (user-reported). `stop_propagation` on click is enough.
                let trailing: AnyElement = if is_hovered {
                    div()
                        .id(SharedString::from(format!("session-tab-close-{id}")))
                        .size(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .hover(|s| s.bg(crate::theme::white_alpha(0.09)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_session_tab(close_id.clone(), cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                        .into_any_element()
                } else {
                    let dot = spaces::status_dot_color(status, &theme);
                    div()
                        .size(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(status != ChatIndicator::Idle, |el| {
                            el.child(div().size(px(6.0)).rounded_full().bg(dot))
                        })
                        .into_any_element()
                };
                let tab_el = div()
                    .id(SharedString::from(format!("session-tab-{id}")))
                    .w(px(SESSION_TAB_WIDTH))
                    .h(px(28.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .rounded(px(8.0))
                    .text_size(px(12.0))
                    .text_color(text_color)
                    .bg(bg)
                    .cursor_pointer()
                    // Tabs sit inside the titlebar drag strip — carve them out.
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    // Track hover in Shell state: the trailing slot flips
                    // between dot and close button (hover_blend only fades
                    // colors; child swaps need real state).
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.tab_hover = Some(hover_id.clone());
                        } else if this.tab_hover.as_deref() == Some(hover_id.as_str()) {
                            this.tab_hover = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.state
                            .update(cx, |s, cx| s.select_chat(Some(select_id.clone()), cx));
                    }))
                    // Middle-click closes (terminal-tab parity).
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_session_tab(middle_id.clone(), cx);
                        }),
                    )
                    .on_drag(
                        TabDragPayload {
                            space: drag_space,
                            from: ix,
                            title: title.clone(),
                            brand,
                        },
                        |payload, _point, _, cx| {
                            let title = payload.title.clone();
                            let brand = payload.brand;
                            cx.stop_propagation();
                            cx.new(|_| TabGhost { title, brand })
                        },
                    )
                    .when_some(brand, |el, (path, tint)| {
                        el.child(
                            icon(path)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(tint.unwrap_or(theme.text_muted).opacity(glyph_alpha)),
                        )
                    })
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(trailing);

                // Sliding transform while a sibling is dragged over: animate
                // 150ms between committed offsets (terminal-panel idiom).
                match drag {
                    Some((from, over, epoch, prev_over)) if ix != from => {
                        let slot = SESSION_TAB_WIDTH + TAB_GAP;
                        let target = slide_offset(ix, from, over) * slot;
                        let start = slide_offset(ix, from, prev_over) * slot;
                        div()
                            .relative()
                            .child(tab_el.with_animation(
                                SharedString::from(format!("session-tab-slide-{id}-{epoch}")),
                                TAB_SLIDE.animation(),
                                move |el, t| el.left(px(motion::lerp(start, target, t))),
                            ))
                            .into_any_element()
                    }
                    // The dragged tab is represented by the cursor ghost; its
                    // flow slot renders as an INVISIBLE spacer. A dimmed tab
                    // here overlapped whatever sibling slid into the vacated
                    // slot (slide_offset moves one tab exactly there —
                    // user-reported double-exposure).
                    Some((from, ..)) if ix == from => div()
                        .w(px(SESSION_TAB_WIDTH))
                        .h(px(28.0))
                        .flex_none()
                        .into_any_element(),
                    _ => tab_el.into_any_element(),
                }
            })
            .collect();

        // `+` — the new-session canvas "is" the unmaterialized tab, so the
        // button carries the active wash while the canvas shows.
        let new_tab = div()
            .id("session-tab-new")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .cursor_pointer()
            .bg(if on_canvas && has_space {
                crate::theme::white_alpha(0.08)
            } else {
                motion::hover_blend(
                    "session-tab-new",
                    gpui::transparent_black(),
                    crate::theme::white_alpha(0.05),
                )
            })
            .on_hover(motion::hover_listener("session-tab-new"))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.route = Route::Chat;
                this.state.update(cx, |s, cx| s.select_chat(None, cx));
                cx.notify();
            }))
            .child(icon(icons::PLUS).size(px(16.0)).text_color(theme.text_muted));

        // Overflow: the tab region scrolls horizontally; edge fades appear on
        // whichever side has hidden tabs (offset from the LAST frame — a
        // one-frame lag is invisible).
        let scrolled = -f32::from(self.tabs_scroll.offset().x);
        let max_scroll = f32::from(self.tabs_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        let bar_bg = theme.bg;
        let drag_move_space = space_id.clone().unwrap_or_default();
        let drop_space = space_id.clone().unwrap_or_default();
        let scroll_for_drag = self.tabs_scroll.clone();
        let tab_region = div()
            .relative()
            .min_w_0()
            .child(
                div()
                    .id("session-tabs-scroll")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(TAB_GAP))
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.tabs_scroll)
                    .on_drag_move::<TabDragPayload>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                            let payload = event.drag(cx);
                            if payload.space != drag_move_space {
                                return;
                            }
                            let from = payload.from;
                            // Drop math runs in CONTENT coordinates: viewport-
                            // relative x plus the scrolled-off width.
                            let rel_x = f32::from(event.event.position.x)
                                - f32::from(event.bounds.left())
                                + -f32::from(scroll_for_drag.offset().x);
                            let over = drop_index(rel_x, SESSION_TAB_WIDTH + TAB_GAP, count);
                            this.update_tab_drag_over(from, over, cx);
                        },
                    ))
                    .on_drop::<TabDragPayload>(cx.listener(
                        move |this, payload: &TabDragPayload, _, cx| {
                            if payload.space != drop_space {
                                this.tab_drag = None;
                                cx.notify();
                                return;
                            }
                            let to = this
                                .tab_drag
                                .as_ref()
                                .map(|d| d.over)
                                .unwrap_or(payload.from);
                            let space = drop_space.clone();
                            this.commit_tab_reorder(&space, payload.from, to, cx);
                        },
                    ))
                    .children(tab_elements),
            )
            .when(fade_left, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pr(px(Theme::SPACE_LG))
            .child(tab_region)
            .when(has_space && has_tabs, |el| el.child(new_tab))
            .child(div().flex_1())
            .when(!self.right_pane_open(cx) && git, |el| {
                el.child(header_icon_button(
                    "toggle-changes",
                    icons::SIDEBAR_MINIMALISTIC,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                ))
            });

        let bar = div()
            .h(px(Theme::HEADER_HEIGHT))
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .child(self.header_inset_container(inner));
        self.titlebar_drag_region("chat-tabs-titlebar", bar, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{next_after_close, resolve_tab_order};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn close_selects_next_then_previous_then_canvas() {
        let order = ids(&["a", "b", "c"]);
        assert_eq!(next_after_close(&order, "a").as_deref(), Some("b"));
        assert_eq!(next_after_close(&order, "b").as_deref(), Some("c"));
        // Last tab: fall back to the previous one.
        assert_eq!(next_after_close(&order, "c").as_deref(), Some("b"));
        // Only tab: canvas.
        assert_eq!(next_after_close(&ids(&["solo"]), "solo"), None);
        // Unknown id: no opinion.
        assert_eq!(next_after_close(&order, "zz"), None);
    }

    #[test]
    fn manual_order_wins_and_new_chats_append() {
        let created = ids(&["a", "b", "c", "d"]);
        // Manual order covers some chats; "gone" no longer exists.
        let manual = ids(&["c", "gone", "a"]);
        assert_eq!(
            resolve_tab_order(&created, &manual),
            ids(&["c", "a", "b", "d"])
        );
        // No manual order → creation order.
        assert_eq!(resolve_tab_order(&created, &[]), created);
        // Manual covers everything → manual order verbatim.
        assert_eq!(
            resolve_tab_order(&ids(&["a", "b"]), &ids(&["b", "a"])),
            ids(&["b", "a"])
        );
    }
}
