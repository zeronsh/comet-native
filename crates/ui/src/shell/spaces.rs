//! Spaces sidebar: the spaces list (folder + device rows), the global
//! Sessions list, and the add-space palette (⌘K-style: device tabs + filtered
//! folder browser).
//!
//! A space = a synced (device, folder) pair; the sidebar's job is switching
//! between them and surfacing which sessions want attention. Child module of
//! `shell` so it renders straight off `Shell`'s private state.

use super::*;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;

/// The add-space palette (a command-K-style surface): device tabs across the
/// top, a search input that filters the folder list, keyboard-first
/// navigation, kbd-hint footer. One surface — switching device tabs rebrowses
/// in place, no step wizard.
pub(super) struct AddSpaceFlow {
    /// The device tab currently browsed.
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED folder rows.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    _search_events: Subscription,
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
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            crate::theme::oklch(0.718, 0.202, 349.761).opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            crate::theme::oklch(0.765, 0.177, 163.223).opacity(0.9) // emerald-400
        }
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
        let (spaces, selected, device_names, offline_devices, attention): (
            Vec<Space>,
            Option<String>,
            std::collections::HashMap<String, String>,
            std::collections::HashSet<String>,
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
            // Host-presence (the revived "Remote" signal): a remote space whose
            // device heartbeat lapsed shows offline — a host outage, not slow sync.
            let offline_devices = spaces
                .iter()
                .map(|s| s.device_id.clone())
                .filter(|id| !state.device_online(id, now))
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
            (
                spaces,
                state.selected_space.clone(),
                device_names,
                offline_devices,
                attention,
            )
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
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
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
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
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
                    let host_offline = offline_devices.contains(&space.device_id);
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let attention = attention.get(&space.id).copied();
                    self.render_space_row(
                        space,
                        device_name,
                        host_offline,
                        is_selected,
                        attention,
                        theme,
                        cx,
                    )
                }),
            ));
        }
        column.into_any_element()
    }

    /// One space row: folder icon + folder name, device name subline.
    /// `host_offline` marks a remote host whose presence heartbeat lapsed.
    #[allow(clippy::too_many_arguments)]
    fn render_space_row(
        &self,
        space: Space,
        device_name: String,
        host_offline: bool,
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
            // Status dot LEADS the row (like session rows) so its position is
            // stable — appearing/disappearing at the right edge made the row
            // jitter (user request). Faint at rest, colored under attention.
            .child(
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .flex_none()
                    .bg(attention
                        .map(|status| status_dot_color(status, theme))
                        .unwrap_or_else(|| crate::theme::white_alpha(0.14))),
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
                    .text_color(if host_offline {
                        theme.warning.opacity(0.8)
                    } else {
                        theme.text_muted.opacity(0.6)
                    })
                    .child(SharedString::from(if host_offline {
                        format!("@ {device_name} · offline")
                    } else {
                        format!("@ {device_name}")
                    })),
            )
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
                            let name = state
                                .device_name(&s.device_id)
                                .unwrap_or("Unknown device")
                                .to_string();
                            // Host-presence: a lapsed heartbeat = host outage,
                            // not slow sync — say so where the session lives.
                            if state.device_online(&s.device_id, now) {
                                name
                            } else {
                                format!("{name} (offline)")
                            }
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

    // ---- add-space flow (the ⌘K-style palette) ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        let devices: Vec<Device> = self.state.read(cx).devices.clone();
        let local = self.state.read(cx).local_device_id.clone();
        // Land on this device's tab (else the first registered device).
        let device = devices
            .iter()
            .find(|d| local.as_deref() == Some(d.id.as_str()))
            .or_else(|| devices.first())
            .cloned();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame instead of moving the text caret.
        let search = cx.new(|cx| ComposerInput::with_context("Search folders…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
            // Enter SELECTS the current folder — it mirrors the footer's
            // "Enter ↵" action. Folder navigation rides ←/→.
            ComposerInputEvent::Submitted => this.submit_add_space(cx),
            _ => {}
        });
        let has_device = device.is_some();
        self.add_space = Some(AddSpaceFlow {
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            _search_events: search_events,
        });
        if has_device {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    /// Device tab click: rebrowse the same palette on another device.
    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.as_ref().is_some_and(|d| d.id == device.id) {
            return;
        }
        flow.device = Some(device);
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(None, cx);
        cx.notify();
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
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
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
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
        let Some(flow) = self.add_space.as_ref() else {
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

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_space
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_space.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_space_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every footer
    /// legend maps to a REAL key: ↑↓ navigate, → open the highlighted folder,
    /// ← up a level, ⏎ select the current folder (the input's own Enter
    /// arrives as Submitted → same submit), ⌫ (empty query) also goes up,
    /// esc closes.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_space_open_active(cx);
                return;
            }
            "left" => {
                self.add_space_go_up(cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_space = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.add_space_filtered(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active =
                        popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => self.submit_add_space(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_space_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: device tabs · search · breadcrumbs · folder list ·
    /// kbd-hint footer with the primary "Use" action.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_space.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (device, search, error, submit_busy, active, loading, load_error, listing, focus, list_scroll) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.device.clone(),
                flow.search.clone(),
                flow.error.clone(),
                flow.submit_busy,
                flow.active,
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.focus.clone(),
                flow.list_scroll.clone(),
            )
        };
        let devices = self.state.read(cx).devices.clone();
        let local_id = self.state.read(cx).local_device_id.clone();
        let rows = self.add_space_filtered(cx);
        let query_empty = search.read(cx).is_empty();
        let hairline = crate::theme::white_alpha(0.06);

        // ── device tabs: EXACTLY the session/terminal tab recipe (h-28
        //    rounded-8 washes in an h-40 row), no platform glyph — the row
        //    reads as one more tab strip, which it is.
        let tabs = div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .children(devices.into_iter().enumerate().map(|(ix, dev)| {
                let is_active = device.as_ref().is_some_and(|d| d.id == dev.id);
                let is_local = local_id.as_deref() == Some(dev.id.as_str());
                let name: SharedString = dev.name.clone().into();
                let pick = dev.clone();
                div()
                    .id(("add-space-device-tab", ix))
                    .h(px(28.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        el.bg(crate::theme::white_alpha(0.08)).text_color(theme.text)
                    })
                    .when(!is_active, |el| {
                        el.text_color(theme.text_muted.opacity(0.6))
                            .hover(|s| s.bg(theme.element_hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_space_pick_device(pick.clone(), cx);
                    }))
                    .child(name)
                    .when(is_local, |el| {
                        el.child(
                            div()
                                .size(px(4.0))
                                .rounded_full()
                                .flex_none()
                                .bg(crate::theme::oklch(0.765, 0.177, 163.223).opacity(0.9)),
                        )
                    })
            }));

        // ── search input row (the ⌘K bar) ───────────────────────────────────
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .px(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .border_b_1()
            .border_color(hairline)
            .child(
                icon(icons::MAGNIFER)
                    .size(px(15.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            );

        // ── breadcrumbs: a quiet mono path line, `/` separators, clickable
        //    ancestors. The root chip is dropped — the leading separator IS
        //    the root (a "/" chip next to a "/" separator read as "//").
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(16.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .children(
                        segments
                            .into_iter()
                            .enumerate()
                            .skip(if last == 0 { 0 } else { 1 })
                            .map(|(ix, (label, full))| {
                                let is_last = ix == last;
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_color(theme.text_faint.opacity(0.7))
                                            .child(SharedString::from("/")),
                                    )
                                    .when(ix > 0, |el| {
                                        el.child({
                                            let crumb = div()
                                                .id(("add-space-crumb", ix))
                                                .px(px(3.0))
                                                .rounded(px(4.0))
                                                .text_color(if is_last {
                                                    theme.text.opacity(0.8)
                                                } else {
                                                    theme.text_muted.opacity(0.55)
                                                })
                                                .child(SharedString::from(label));
                                            if is_last {
                                                crumb.into_any_element()
                                            } else {
                                                crumb
                                                    .cursor_pointer()
                                                    .hover(|s| s.text_color(Theme::dark().text))
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            if let Some(flow) =
                                                                this.add_space.as_mut()
                                                            {
                                                                flow.browser_repo = false;
                                                            }
                                                            this.load_space_folders(
                                                                Some(full.clone()),
                                                                cx,
                                                            );
                                                        },
                                                    ))
                                                    .into_any_element()
                                            }
                                        })
                                    })
                            }),
                    )
                    .into_any_element()
            }
            None => div().pt(px(6.0)).into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows("add-space-skeleton", &theme, 6))
                .into_any_element()
        } else if let Some(message) = load_error {
            let device_line = device
                .as_ref()
                .map(|d| format!("{} didn't respond — is it online?", d.name))
                .unwrap_or(message);
            popover::error_row(&theme, &device_line)
                .px(px(14.0))
                .py(px(10.0))
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
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            div()
                .id("add-space-folders")
                .max_h(px(302.0))
                .overflow_y_scroll()
                .track_scroll(&list_scroll)
                .px(px(8.0))
                .py(px(6.0))
                .flex()
                .flex_col()
                // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                .gap(px(2.0))
                .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                    let name: SharedString = entry.name.clone().into();
                    let full = crate::pickers::child_path(&base_path, &entry.name);
                    let is_repo = entry.is_repo;
                    popover::menu_row_nav(&theme, false, ix == active, format!("add-space-folder-{ix}"))
                        .id(("add-space-folder", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add_space_descend(full.clone(), is_repo, cx);
                        }))
                        .child(
                            icon(icons::FOLDER)
                                .size(px(15.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.8)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                }))
                .into_any_element()
        };

        // ── footer: icon key-caps (Solar set, same pack as the rest of the
        //    app) + tiny verbs, one compact 22px row shared with Select.
        let hint = |icon_path: &'static str, label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.0))
                .child(
                    div()
                        .h(px(22.0))
                        .px(px(5.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(crate::theme::white_alpha(0.05))
                        .child(
                            icon(icon_path)
                                .size(px(12.5))
                                .text_color(theme.text_muted.opacity(0.7)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_muted.opacity(0.45))
                        .child(SharedString::from(label)),
                )
        };
        let footer = div()
            .flex_none()
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(hint(icons::SORT_VERTICAL, "Navigate"))
            .child(hint(icons::ARROW_LEFT, "Up"))
            .child(hint(icons::ARROW_RIGHT, "Open"))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            })
            .child(div().flex_1())
            .child(
                popover::btn_primary(&theme, if submit_busy { "Adding…" } else { "Enter" })
                    .id("add-space-submit")
                    .h(px(22.0))
                    .px(px(9.0))
                    .py(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(12.0))
                    .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
                    .on_click(cx.listener(|this, _, _, cx| this.submit_add_space(cx)))
                    .child(
                        icon(icons::RETURN)
                            .size(px(12.0))
                            .text_color(crate::theme::grey(0x0e).opacity(0.8)),
                    ),
            );

        let card = div()
            .id("add-space-palette")
            .w(px(560.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(crate::theme::white_alpha(0.10))
            .bg(crate::theme::grey(0x10))
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_color(theme.text)
            // On the keyboard dispatch path (see `AddSpaceFlow::focus`) — the
            // pickers' proven structure for frame-level keys with a focused
            // child input.
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.add_space_key(event, cx)
            }))
            // Clicking the scrim dismisses (user requirement) — same close
            // path as Escape.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.add_space = None;
                cx.notify();
            }))
            .child(tabs)
            .child(input_row)
            .child(crumbs)
            .child(list)
            .child(footer)
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
