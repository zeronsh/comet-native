//! Settings → Devices (feature-inventory §1.5): the device registry — name,
//! platform, last-seen, presence dot, a "This device" badge, click-to-copy id,
//! and a Rename dialog (Mutate renameDevice).

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use std::time::Duration;

use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::state::AppState;
use crate::theme::Theme;

/// A device that pinged within this window shows a presence dot (engines
/// heartbeat every 15s; 70s tolerates a couple of missed beats).
pub const DEVICE_ONLINE_WINDOW_SECS: i64 = 70;

/// Presence: last-seen within the online window (future timestamps count). Pure.
pub fn device_online(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_seen
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= DEVICE_ONLINE_WINDOW_SECS)
}

/// Compact last-seen line. Pure.
pub fn format_last_seen(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = last_seen else {
        return "never seen".to_string();
    };
    let secs = now.signed_duration_since(at).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

struct RenameDialog {
    device_id: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct DevicesPage {
    state: Entity<AppState>,
    rename: Option<RenameDialog>,
    /// Device id whose id-chip shows "Copied" right now.
    copied: Option<String>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _observe: Subscription,
}

impl DevicesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            rename: None,
            copied: None,
            error: None,
            task: None,
            copy_task: None,
            _observe: observe,
        }
    }

    fn open_rename(&mut self, device_id: String, current: String, cx: &mut Context<Self>) {
        let input = cx.new(|cx| ComposerInput::new("Device name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename(cx);
            }
        });
        self.rename = Some(RenameDialog {
            device_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({
            "op": "renameDevice",
            "deviceId": dialog.device_id,
            "name": name,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(format!("Rename failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn copy_id(&mut self, device_id: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(device_id.clone()));
        self.copied = Some(device_id);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_rename_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.rename.as_ref()?;
        let input = dialog.input.clone();
        let card = div()
            .w(px(360.0))
            .p(px(Theme::SPACE_LG))
            .rounded(px(Theme::PANEL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .child(SharedString::from("Rename device")),
            )
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .child(input),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .id("rename-cancel")
                            .px(px(Theme::SPACE_MD))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename = None;
                                cx.notify();
                            }))
                            .child(SharedString::from("Cancel")),
                    )
                    .child(
                        div()
                            .id("rename-save")
                            .px(px(Theme::SPACE_MD))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .bg(theme.accent_strong)
                            .text_size(px(12.0))
                            .text_color(gpui::white())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx)))
                            .child(SharedString::from("Save")),
                    ),
            )
            .into_any_element();
        Some(popover::modal("rename-device-dialog", card))
    }
}

impl Render for DevicesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let (devices, local_id) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let copied = self.copied.clone();
        let dialog = self.render_rename_dialog(cx);

        let rows: Vec<AnyElement> = devices
            .into_iter()
            .enumerate()
            .map(|(ix, device)| {
                let online = device_online(device.last_seen_at, now);
                let is_local = local_id.as_deref() == Some(device.id.as_str());
                let id_copied = copied.as_deref() == Some(device.id.as_str());
                let id_chip: SharedString = if id_copied {
                    "Copied".into()
                } else {
                    let id = &device.id;
                    if id.len() > 14 {
                        format!("{}…", &id[..12]).into()
                    } else {
                        id.clone().into()
                    }
                };
                let copy_id = device.id.clone();
                let rename_id = device.id.clone();
                let rename_name = device.name.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_MD))
                    .px(px(Theme::SPACE_MD))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(theme.border)
                    // Presence dot.
                    .child(
                        div()
                            .size(px(7.0))
                            .rounded_full()
                            .flex_none()
                            .bg(if online {
                                theme.accent
                            } else {
                                theme.border_strong
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .text_color(theme.text)
                                            .truncate()
                                            .child(SharedString::from(device.name.clone())),
                                    )
                                    .when(is_local, |el| {
                                        el.child(
                                            div()
                                                .px(px(5.0))
                                                .rounded(px(4.0))
                                                .border_1()
                                                .border_color(theme.accent)
                                                .text_size(px(9.0))
                                                .text_color(theme.accent)
                                                .child(SharedString::from("This device")),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(format!(
                                        "{} · {}",
                                        device.platform,
                                        format_last_seen(device.last_seen_at, now)
                                    ))),
                            ),
                    )
                    // Click-to-copy id.
                    .child(
                        div()
                            .id(("device-id", ix))
                            .px(px(Theme::SPACE_SM))
                            .py(px(3.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(10.0))
                            .font_family(theme.font_mono.clone())
                            .text_color(if id_copied {
                                theme.accent
                            } else {
                                theme.text_faint
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_id(copy_id.clone(), cx);
                            }))
                            .child(id_chip),
                    )
                    .child(
                        div()
                            .id(("device-rename", ix))
                            .px(px(Theme::SPACE_SM))
                            .py(px(3.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename(rename_id.clone(), rename_name.clone(), cx);
                            }))
                            .child(SharedString::from("Rename")),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .id("devices-page")
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
                    .child(SharedString::from("Devices")),
            )
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div()
                        .id("devices-error")
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
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("No devices registered yet")),
                )
            })
            .children(rows)
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn presence_window() {
        let now = Utc::now();
        assert!(device_online(Some(now - TimeDelta::seconds(10)), now));
        assert!(device_online(Some(now - TimeDelta::seconds(70)), now));
        assert!(!device_online(Some(now - TimeDelta::seconds(71)), now));
        assert!(!device_online(None, now));
        // Clock skew (future) counts as online.
        assert!(device_online(Some(now + TimeDelta::seconds(30)), now));
    }

    #[test]
    fn last_seen_formatting() {
        let now = Utc::now();
        assert_eq!(format_last_seen(None, now), "never seen");
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::seconds(30)), now),
            "just now"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::hours(3)), now),
            "3h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::days(2)), now),
            "2d ago"
        );
    }
}
