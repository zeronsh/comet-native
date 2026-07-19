//! Settings → Archived (feature-inventory §1.5): archived chats across
//! devices, with Unarchive (Mutate setChatArchived false).

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use comet_proto::Chat;
use comet_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

/// Archived rows in sidebar (recency) order. Pure.
pub fn archived_chats(chats: &[Chat]) -> Vec<&Chat> {
    chats.iter().filter(|c| c.archived).collect()
}

pub struct ArchivedPage {
    state: Entity<AppState>,
    error: Option<SharedString>,
    /// Chat with an in-flight unarchive (button shows working state).
    busy: Option<String>,
    task: Option<Task<()>>,
    _observe: Subscription,
}

impl ArchivedPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self { state, error: None, busy: None, task: None, _observe: observe }
    }

    fn unarchive(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else { return };
        self.busy = Some(chat_id.clone());
        self.error = None;
        let params = serde_json::json!({
            "op": "setChatArchived",
            "chatId": chat_id,
            "archived": false,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                if let Err(err) = result {
                    page.error = Some(format!("Unarchive failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for ArchivedPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let (rows, device_names): (Vec<Chat>, std::collections::HashMap<String, String>) = {
            let state = self.state.read(cx);
            let rows = archived_chats(&state.chats).into_iter().cloned().collect();
            let names = state.devices.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
            (rows, names)
        };
        let busy = self.busy.clone();

        let items: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, chat)| {
                let title: SharedString =
                    chat.title.clone().unwrap_or_else(|| "Untitled session".into()).into();
                let device: SharedString = device_names
                    .get(&chat.device_id)
                    .cloned()
                    .unwrap_or_else(|| chat.device_id.clone())
                    .into();
                let is_busy = busy.as_deref() == Some(chat.id.as_str());
                let chat_id = chat.id.clone();
                let preview: Option<SharedString> =
                    chat.last_message_preview.clone().map(Into::into);
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_MD))
                    .px(px(Theme::SPACE_MD))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.text)
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(6.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(device)
                                    .when_some(preview, |el, preview| {
                                        el.child(
                                            div().min_w_0().truncate().child(preview),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id(("unarchive", ix))
                            .px(px(Theme::SPACE_SM))
                            .py(px(3.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(11.0))
                            .text_color(theme.text)
                            .when(is_busy, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.unarchive(chat_id.clone(), cx);
                            }))
                            .child(SharedString::from(if is_busy { "Unarchiving…" } else { "Unarchive" })),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .id("archived-page")
            .size_full()
            .overflow_y_scroll()
            .p(px(Theme::SPACE_LG))
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme.text)
                    .child(SharedString::from("Archived chats")),
            )
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div()
                        .id("archived-error")
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        }))
                        .child(message),
                )
            })
            .when(items.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Nothing archived")),
                )
            })
            .children(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, archived: bool) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![chat("a", false), chat("b", true), chat("c", true)];
        let rows = archived_chats(&chats);
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }
}
