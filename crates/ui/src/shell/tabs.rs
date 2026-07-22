//! The session tab strip — replaces the chat header (feature spec: spaces
//! overhaul). Every non-archived session of the selected space is a tab:
//! agent brand icon + title + a trailing slot that shows the status dot at
//! rest and swaps to a close button on hover. `+` at the end opens the
//! new-session canvas (the tab materializes on first send). The strip inherits
//! the old header's titlebar duties: 44px tall, drag region, animated
//! window-controls inset, and the toggle-changes button (git spaces only).
//!
//! Styling mirrors the terminal tab bar (`terminal/panel.rs::render_tab_bar`)
//! — fixed-width rounded tabs so a later drag-reorder can reuse its math.

use super::*;
use comet_proto::ChatIndicator;

/// Fixed tab width (terminal tabs use 118; session titles get a bit more).
pub(super) const SESSION_TAB_WIDTH: f32 = 140.0;

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
    /// Close a tab = archive the session. Selection moves to a neighbor; the
    /// last tab lands on the new-session canvas.
    pub(super) fn close_session_tab(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let (selected, order) = {
            let state = self.state.read(cx);
            let order: Vec<String> = state
                .selected_space
                .as_deref()
                .map(|space| {
                    state
                        .chats_in_space(space)
                        .iter()
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            (state.selected_chat.clone(), order)
        };
        if selected.as_deref() == Some(chat_id.as_str()) {
            let next = next_after_close(&order, &chat_id);
            self.state.update(cx, |s, cx| s.select_chat(next, cx));
        }
        self.archive_chat(chat_id, cx);
    }

    /// The tab strip: [scrollable tabs][+][drag spacer][toggle-changes].
    pub(super) fn render_session_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let (tabs, selected, has_space): (
            Vec<(String, SharedString, Option<comet_proto::HarnessId>, ChatIndicator)>,
            Option<String>,
            bool,
        ) = {
            let state = self.state.read(cx);
            let tabs = state
                .selected_space
                .as_deref()
                .map(|space| {
                    state
                        .chats_in_space(space)
                        .into_iter()
                        .map(|chat| {
                            (
                                chat.id.clone(),
                                SharedString::from(transcript::single_line(
                                    &chat.title.clone().unwrap_or_else(|| "New session".into()),
                                )),
                                chat.config.as_ref().map(|c| c.harness),
                                state.display_status_for(chat, now),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                tabs,
                state.selected_chat.clone(),
                state.selected_space.is_some(),
            )
        };
        let git = self.space_git_detected(cx);
        let hovered = self.tab_hover.clone();
        let on_canvas = selected.is_none();

        let tab_elements: Vec<AnyElement> = tabs
            .into_iter()
            .map(|(id, title, harness, status)| {
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
                // Trailing 20px slot: status dot at rest ↔ close on hover.
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
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
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
                div()
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
                    .when_some(brand, |el, (path, tint)| {
                        el.child(
                            icon(path)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(tint.unwrap_or(theme.text_muted).opacity(glyph_alpha)),
                        )
                    })
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(trailing)
                    .into_any_element()
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

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pr(px(Theme::SPACE_LG))
            .child(
                div()
                    .id("session-tabs-scroll")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tab_elements),
            )
            .when(has_space, |el| el.child(new_tab))
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
    use super::next_after_close;

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
}
