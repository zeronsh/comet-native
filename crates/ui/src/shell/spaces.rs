//! Spaces sidebar: the spaces list (folder + device rows), the global Active
//! sessions list, and the add-space picker flow (device → folder browser).
//!
//! A space = a synced (device, folder) pair; the sidebar's job is switching
//! between them and surfacing which sessions want attention. Child module of
//! `shell` so it renders straight off `Shell`'s private state.

use super::*;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;

/// Which step of the add-space flow is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddSpaceStep {
    PickDevice,
    Browse,
}

/// The add-space picker (modal): pick a device, browse its folders, confirm.
pub(super) struct AddSpaceFlow {
    step: AddSpaceStep,
    device: Option<Device>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    submit_busy: bool,
    error: Option<SharedString>,
    focus: FocusHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        ChatIndicator::Working => {
            crate::theme::oklch(0.879, 0.169, 91.605).opacity(0.8) // amber-300
        }
        // Blue, not amber: "asking you a question" must read differently from
        // "busy working" at a glance (user request).
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Finished-but-unseen: a solid bright dot, distinct from the amber
        // live states and the faint idle rail.
        ChatIndicator::Completed => theme.text.opacity(0.9),
        ChatIndicator::Idle => crate::theme::white_alpha(0.14),
    }
}

impl Shell {
    // ---- space switching ----

    /// Land in a space: remembered tab if alive, else the most recent chat in
    /// the space, else the new-session canvas. Persists `last_space_id`.
    pub(super) fn activate_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id.clone()), cx);
        });
        let target = {
            let state = self.state.read(cx);
            let in_space = |id: &str| {
                state
                    .visible_chats()
                    .any(|c| c.id == id && c.space_id.as_deref() == Some(space_id.as_str()))
            };
            self.space_last_chat
                .get(&space_id)
                .filter(|id| in_space(id))
                .cloned()
                .or_else(|| {
                    // `visible_chats` is recency-sorted — first match is the
                    // most recent chat of the space.
                    state
                        .visible_chats()
                        .find(|c| c.space_id.as_deref() == Some(space_id.as_str()))
                        .map(|c| c.id.clone())
                })
        };
        self.state.update(cx, |s, cx| s.select_chat(target, cx));
        self.settings.last_space_id = Some(space_id);
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- sidebar sections ----

    /// The "Spaces" section: tracked header + add button, then a row per space.
    pub(super) fn render_spaces_section(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (spaces, selected, device_names, attention): (
            Vec<Space>,
            Option<String>,
            std::collections::HashMap<String, String>,
            std::collections::HashMap<String, ChatIndicator>,
        ) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            let spaces = state.spaces.clone();
            let device_names = spaces
                .iter()
                .map(|s| {
                    (
                        s.device_id.clone(),
                        state
                            .device_name(&s.device_id)
                            .unwrap_or("Unknown device")
                            .to_string(),
                    )
                })
                .collect();
            // Spaces with a live/awaiting session get an aggregate dot (the
            // most urgent member status wins) so the attention signal survives
            // even with the Sessions list scrolled off.
            let mut attention: std::collections::HashMap<String, ChatIndicator> =
                std::collections::HashMap::new();
            for chat in state.visible_chats() {
                let status = state.display_status_for(chat, now);
                if !matches!(
                    status,
                    ChatIndicator::Working | ChatIndicator::AwaitingInput
                ) {
                    continue;
                }
                let Some(space_id) = chat.space_id.clone() else {
                    continue;
                };
                attention
                    .entry(space_id)
                    .and_modify(|held| {
                        if crate::state::attention_rank(status)
                            < crate::state::attention_rank(*held)
                        {
                            *held = status;
                        }
                    })
                    .or_insert(status);
            }
            (spaces, state.selected_space.clone(), device_names, attention)
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Spaces")),
            )
            .child(
                div()
                    .id("add-space")
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "add-space",
                        gpui::transparent_black(),
                        crate::theme::white_alpha(0.06),
                    ))
                    .on_hover(motion::hover_listener("add-space"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_space(window, cx)))
                    .child(
                        icon(icons::PLUS)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );

        let mut column = div().flex().flex_col().child(header);
        if spaces.is_empty() {
            // Ghost row: the empty-state affordance mirrors a space row.
            column = column.child(
                div()
                    .id("add-space-ghost")
                    .mx(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(motion::hover_blend(
                        "add-space-ghost",
                        theme.text_muted,
                        Theme::dark().text,
                    ))
                    .bg(motion::hover_blend(
                        "add-space-ghost",
                        gpui::transparent_black(),
                        Theme::dark().element_hover,
                    ))
                    .on_hover(motion::hover_listener("add-space-ghost"))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_space(window, cx)))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Add space")),
            );
        } else {
            column = column.child(div().flex().flex_col().gap(px(2.0)).children(
                spaces.into_iter().map(|space| {
                    let device_name = device_names
                        .get(&space.device_id)
                        .cloned()
                        .unwrap_or_else(|| "Unknown device".to_string());
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let attention = attention.get(&space.id).copied();
                    self.render_space_row(space, device_name, is_selected, attention, theme, cx)
                }),
            ));
        }
        column.into_any_element()
    }

    /// One space row: folder icon + folder name, device name subline.
    fn render_space_row(
        &self,
        space: Space,
        device_name: String,
        selected: bool,
        attention: Option<ChatIndicator>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = space.id.clone();
        let name: SharedString = space.display_name().to_string().into();
        let fade_key = format!("space-row-{id}");
        let rest_bg = if selected {
            crate::theme::white_alpha(0.08)
        } else {
            gpui::transparent_black()
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let menu_id = id.clone();
        // One line: "name @ device" — the folder name carries the weight, the
        // device tag rides along slightly muted. Long names truncate; the
        // device tag stays visible.
        div()
            .id(SharedString::from(format!("space-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_space(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.space_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(format!("@ {device_name}"))),
            )
            .when_some(attention, |el, status| {
                el.child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .flex_none()
                        .bg(status_dot_color(status, theme)),
                )
            })
            .into_any_element()
    }

    /// The global "Sessions" list: every session across all spaces (idle
    /// included), attention-sorted. Rows are keyed for the FLIP resort glide.
    pub(super) fn render_active_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let rows: Vec<(ChatIndicator, comet_proto::Chat, String)> = {
            let state = self.state.read(cx);
            state
                .overview_chats(now)
                .into_iter()
                .map(|(status, chat)| {
                    let space = state.space_for_chat(chat);
                    let folder = space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let device = space
                        .map(|s| {
                            state
                                .device_name(&s.device_id)
                                .unwrap_or("Unknown device")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let mut location = format!("{folder} · {device}");
                    if let Some(branch) = chat.branch.as_deref().filter(|b| !b.trim().is_empty()) {
                        location = format!("{folder} · {branch} · {device}");
                    }
                    (status, chat.clone(), location)
                })
                .collect()
        };
        let selected = self.state.read(cx).selected_chat.clone();
        rows.into_iter()
            .map(|(status, chat, location)| {
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let is_selected = selected.as_deref() == Some(chat.id.as_str());
                let element = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )
                    .into(),
                    time_ago,
                    Some(location.into()),
                    status,
                    is_selected,
                    theme,
                    cx,
                );
                (format!("c:{}", chat.id), super::CHAT_ROW_WITH_LOCATION_HEIGHT, element)
            })
            .collect()
    }

    // ---- add-space flow ----

    pub(super) fn open_add_space(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let devices: Vec<Device> = self.state.read(cx).devices.clone();
        let single = (devices.len() == 1).then(|| devices[0].clone());
        let browse = single.is_some();
        self.add_space = Some(AddSpaceFlow {
            step: if browse {
                AddSpaceStep::Browse
            } else {
                AddSpaceStep::PickDevice
            },
            device: single,
            browser: Loadable::Idle,
            browser_path: None,
            browser_repo: false,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
        });
        if browse {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        if let Some(flow) = self.add_space.as_mut() {
            flow.device = Some(device);
            flow.step = AddSpaceStep::Browse;
            flow.browser = Loadable::Idle;
            flow.browser_path = None;
            flow.browser_repo = false;
            flow.error = None;
        }
        self.load_space_folders(None, cx);
        cx.notify();
    }

    /// ListFolders on the flow's device (relay-forwarded when remote).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            // Only target remote devices — local calls skip the relay.
            if let (Some(target), local) = (&device_id, &local)
                && local.as_deref() != Some(target.as_str())
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => Loadable::Ready(listing),
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the space for the browser's current folder.
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        // Same (device, folder) already has a space → just switch to it. The
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if let Some(existing) = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| s.device_id == device.id && s.path == path)
            .map(|s| s.id.clone())
        {
            self.add_space = None;
            self.activate_space(existing, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = space_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_space = None;
                        shell.activate_space(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        match (key, flow.step) {
            ("escape", AddSpaceStep::PickDevice) => {
                self.add_space = None;
                cx.notify();
            }
            ("escape", AddSpaceStep::Browse) => {
                // Back to the device step (or close for single-device setups).
                let devices = self.state.read(cx).devices.len();
                if devices > 1 {
                    if let Some(flow) = self.add_space.as_mut() {
                        flow.step = AddSpaceStep::PickDevice;
                        flow.error = None;
                    }
                } else {
                    self.add_space = None;
                }
                cx.notify();
            }
            ("backspace", AddSpaceStep::Browse) => {
                if let Some(parent) = flow
                    .browser
                    .ready()
                    .and_then(|l| parent_path(&l.path))
                {
                    if let Some(flow) = self.add_space.as_mut() {
                        flow.browser_repo = false; // unknown at the parent
                    }
                    self.load_space_folders(Some(parent), cx);
                }
            }
            ("enter", AddSpaceStep::Browse)
                if event.keystroke.modifiers.platform || event.keystroke.modifiers.control =>
            {
                self.submit_add_space(cx);
            }
            _ => {}
        }
    }

    /// The add-space modal card (device step or browser step).
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let flow = self.add_space.as_mut()?;
        if std::mem::take(&mut flow.focus_pending) {
            window.focus(&flow.focus, cx);
        }
        let step = flow.step;
        let device = flow.device.clone();
        let error = flow.error.clone();
        let submit_busy = flow.submit_busy;
        let focus = flow.focus.clone();

        let body: AnyElement = match step {
            AddSpaceStep::PickDevice => {
                let devices = self.state.read(cx).devices.clone();
                let local = self.state.read(cx).local_device_id.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .mt(px(10.0))
                    .children(devices.into_iter().enumerate().map(|(ix, d)| {
                        let glyph = match d.platform.as_str() {
                            "macos" | "darwin" => icons::LAPTOP,
                            _ => icons::MONITOR,
                        };
                        let is_local = local.as_deref() == Some(d.id.as_str());
                        let name: SharedString = d.name.clone().into();
                        let subline: SharedString = if is_local {
                            format!("{} · this device", d.platform).into()
                        } else {
                            d.platform.clone().into()
                        };
                        let pick = d.clone();
                        popover::menu_row_nav(&theme, false, false, format!("add-space-device-{ix}"))
                            .id(("add-space-device", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_pick_device(pick.clone(), cx);
                            }))
                            .child(
                                icon(glyph)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().truncate().child(name))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(subline),
                                    ),
                            )
                    }))
                    .into_any_element()
            }
            AddSpaceStep::Browse => {
                let listing = self.add_space.as_ref().and_then(|f| f.browser.ready().cloned());
                let loading = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.browser.is_loading());
                let load_error = self
                    .add_space
                    .as_ref()
                    .and_then(|f| f.browser.error().map(str::to_string));
                let crumbs: AnyElement = match &listing {
                    Some(listing) => {
                        let segments = breadcrumbs(&listing.path);
                        let last = segments.len().saturating_sub(1);
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(px(2.0))
                            .mt(px(10.0))
                            .px(px(2.0))
                            .children(segments.into_iter().enumerate().map(|(ix, (label, full))| {
                                let is_last = ix == last;
                                let crumb = div()
                                    .id(("add-space-crumb", ix))
                                    .px(px(4.0))
                                    .py(px(1.0))
                                    .rounded(px(5.0))
                                    .text_size(px(11.0))
                                    .text_color(if is_last {
                                        theme.text
                                    } else {
                                        theme.text_muted.opacity(0.7)
                                    })
                                    .child(SharedString::from(if label == "/" && ix == 0 {
                                        "/".to_string()
                                    } else {
                                        label
                                    }));
                                if is_last {
                                    crumb.into_any_element()
                                } else {
                                    crumb
                                        .cursor_pointer()
                                        .hover(|s| s.bg(crate::theme::white_alpha(0.06)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(flow) = this.add_space.as_mut() {
                                                flow.browser_repo = false;
                                            }
                                            this.load_space_folders(Some(full.clone()), cx);
                                        }))
                                        .into_any_element()
                                }
                            }))
                            .into_any_element()
                    }
                    None => div().mt(px(10.0)).into_any_element(),
                };
                let rows: AnyElement = if loading {
                    div()
                        .mt(px(6.0))
                        .child(popover::skeleton_rows("add-space-skeleton", &theme, 5))
                        .into_any_element()
                } else if let Some(message) = load_error {
                    let device_line = device
                        .as_ref()
                        .map(|d| format!("{} didn't respond — is it online?", d.name))
                        .unwrap_or_else(|| message.clone());
                    popover::error_row(&theme, &device_line)
                        .mt(px(6.0))
                        .child(
                            div()
                                .id("add-space-retry")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let path =
                                        this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                                    this.load_space_folders(path, cx);
                                }))
                                .child(SharedString::from("Retry")),
                        )
                        .into_any_element()
                } else if let Some(listing) = &listing {
                    let dirs = browser_rows(listing);
                    if dirs.is_empty() {
                        div()
                            .mt(px(6.0))
                            .p(px(Theme::SPACE_SM))
                            .text_size(px(12.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("No folders here"))
                            .into_any_element()
                    } else {
                        let base = listing.path.clone();
                        div()
                            .id("add-space-folders")
                            .mt(px(6.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .max_h(px(280.0))
                            .overflow_y_scroll()
                            .children(dirs.into_iter().enumerate().map(|(ix, entry)| {
                                let name: SharedString = entry.name.clone().into();
                                let full = crate::pickers::child_path(&base, &entry.name);
                                let is_repo = entry.is_repo;
                                popover::menu_row_nav(
                                    &theme,
                                    false,
                                    false,
                                    format!("add-space-folder-{ix}"),
                                )
                                .id(("add-space-folder", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(flow) = this.add_space.as_mut() {
                                        flow.browser_repo = is_repo;
                                    }
                                    this.load_space_folders(Some(full.clone()), cx);
                                }))
                                .child(
                                    icon(icons::FOLDER)
                                        .size(px(15.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(name))
                                .when(is_repo, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(5.0))
                                            .bg(crate::theme::white_alpha(0.05))
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(SharedString::from("git")),
                                    )
                                })
                            }))
                            .into_any_element()
                    }
                } else {
                    div().into_any_element()
                };
                let pick_label: SharedString = listing
                    .as_ref()
                    .map(|l| {
                        let name = std::path::Path::new(l.path.trim_end_matches('/'))
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| l.path.clone());
                        format!("Use “{name}”").into()
                    })
                    .unwrap_or_else(|| "Use this folder".into());
                div()
                    .flex()
                    .flex_col()
                    .child(crumbs)
                    .child(rows)
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(6.0))
                                .px(px(4.0))
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(14.0))
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(
                                popover::btn_ghost(&theme, "Back", "add-space-back")
                                    .id("add-space-back")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let multi = this.state.read(cx).devices.len() > 1;
                                        if multi {
                                            if let Some(flow) = this.add_space.as_mut() {
                                                flow.step = AddSpaceStep::PickDevice;
                                                flow.error = None;
                                            }
                                        } else {
                                            this.add_space = None;
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submit_busy { "Adding…" } else { &pick_label },
                                )
                                .id("add-space-submit")
                                .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_add_space(cx))),
                            ),
                    )
                    .into_any_element()
            }
        };

        let subtitle: SharedString = match step {
            AddSpaceStep::PickDevice => "Choose the device this space lives on.".into(),
            AddSpaceStep::Browse => device
                .as_ref()
                .map(|d| format!("Pick a folder on {}.", d.name).into())
                .unwrap_or_else(|| "Pick a folder.".into()),
        };
        let card = popover::dialog_card(&theme)
            .w(px(420.0))
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.add_space_key(event, cx)
            }))
            .child(popover::dialog_title(&theme, "Add a space"))
            .child(div().mt(px(4.0)).child(popover::dialog_body(&theme, subtitle)))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-space-dialog", viewport, card))
    }

    // ---- space context menu / rename / delete overlays ----

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.space_menu = None;
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Space name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.clone() {
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.space_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.space_menu = None;
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("space-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename space"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this space".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove space?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}

