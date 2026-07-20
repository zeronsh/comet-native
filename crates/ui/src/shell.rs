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
    AnyElement, App, Context, Empty, Entity, IntoElement, KeyBinding, Keystroke, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Subscription, Task, Window,
    actions, div, prelude::*, px,
};

use comet_rpc::methods;

use crate::changes::Changes;
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, RESIZE, SPLASH_OUT};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::accounts::AccountsPage;
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, TERMINAL_DEFAULT_HEIGHT, UiSettings, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, EngineMode, GatePhase, Indicator, OrgRow,
    group_chats, org_name_valid, parse_orgs, sort_memberships,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript};

actions!(shell, [ToggleSidebar, ToggleChanges]);

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
    ]);
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    Agents,
    Shortcuts,
    Archived,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::Devices,
        SettingsSection::Agents,
        SettingsSection::Shortcuts,
        SettingsSection::Archived,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::Agents => "Agents",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Archived",
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

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

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// The "Create your workspace" gate (feature-inventory §1.2 OrgGate).
struct OrgGateUi {
    name_input: Entity<ComposerInput>,
    orgs: Loadable<Vec<OrgRow>>,
    submitting: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _events: Subscription,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    changes: Option<Entity<Changes>>,
    /// Chat outlet vs settings pages.
    route: Route,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    shortcuts_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: Option<(String, Point<Pixels>)>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    user_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    user_menu_dismissed_at: Option<std::time::Instant>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    org: Option<OrgGateUi>,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    terminal_tween: Option<WidthTween>,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    tween_epoch: usize,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Own-send re-engages the stick-to-bottom pin with a smooth scroll.
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent { .. } => {
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
            }
        });
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        Self {
            state,
            transcript,
            composer,
            terminal: None,
            changes: None,
            route: Route::Chat,
            devices_page: None,
            archived_page: None,
            shortcuts_page: None,
            accounts_page: None,
            shortcuts_sub: None,
            chat_menu: None,
            rename_dialog: None,
            delete_confirm: None,
            user_menu_open: false,
            user_menu_dismissed_at: None,
            sidebar_notice: None,
            org: None,
            mutate_task: None,
            auth_task: None,
            boot,
            data_dir,
            settings,
            sidebar_tween: None,
            right_tween: None,
            terminal_tween: None,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            tween_epoch: 0,
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
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
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    fn right_target(&self) -> f32 {
        if self.settings.right_pane_open {
            self.settings.right_pane_width
        } else {
            0.0
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.tween_epoch += 1;
        self.sidebar_tween = Some(WidthTween {
            from,
            to: self.sidebar_target(),
            epoch: self.tween_epoch,
        });
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target();
        self.settings.right_pane_open = !self.settings.right_pane_open;
        self.tween_epoch += 1;
        self.right_tween = Some(WidthTween {
            from,
            to: self.right_target(),
            epoch: self.tween_epoch,
        });
        if self.settings.right_pane_open {
            // Lazy: the Changes entity (and its WatchCheckoutDiffs) exists only
            // once the pane has been opened.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.changes = Some(changes.clone());
        changes
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self) -> f32 {
        if self.settings.terminal_open {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    fn toggle_terminal(&mut self, cx: &mut Context<Self>) {
        let from = self.terminal_target();
        self.settings.terminal_open = !self.settings.terminal_open;
        self.tween_epoch += 1;
        self.terminal_tween = Some(WidthTween {
            from,
            to: self.terminal_target(),
            epoch: self.tween_epoch,
        });
        let open = self.settings.terminal_open;
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total() + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
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
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
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

    // ---- routes / settings ----

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn toggle_grouped(&mut self, cx: &mut Context<Self>) {
        self.settings.sidebar_grouped = !self.settings.sidebar_grouped;
        self.schedule_save(cx);
        cx.notify();
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": true }),
            cx,
        );
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await
            {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("Sign out failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                }
                Err(err) => {
                    shell.sidebar_notice = Some(format!("Sign in failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ---- org gate ----

    fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        let name_input = cx.new(|cx| ComposerInput::new("Workspace name", cx));
        let events = cx.subscribe(&name_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.create_org(cx);
            }
        });
        self.org = Some(OrgGateUi {
            name_input,
            orgs: Loadable::Idle,
            submitting: false,
            error: None,
            task: None,
            _events: events,
        });
        self.load_orgs(cx);
    }

    fn load_orgs(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.orgs = Loadable::Loading;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ORGS, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.orgs = match result {
                        Ok(value) => Loadable::Ready(sort_memberships(parse_orgs(&value))),
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn create_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        let name = org.name_input.read(cx).text().trim().to_string();
        if !org_name_valid(&name) {
            org.error = Some("Enter a workspace name".into());
            cx.notify();
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CREATE_ORG, serde_json::json!({ "name": name }))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                    // Success: the AuthStatus stream flips to SignedIn and the
                    // gate falls away on its own.
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select_org(&mut self, organization_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SELECT_ORG,
                    serde_json::json!({ "organizationId": organization_id }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        self.pane_container(
            "sidebar-width",
            self.sidebar_tween,
            target,
            div()
                .h_full()
                .bg(theme.surface)
                .when(target > 0.0, |el| {
                    el.border_r_1().border_color(theme.border)
                })
                .child(inner)
                .into_any_element(),
        )
    }

    /// Settings-mode sidebar: back to chats + section nav (§1.5).
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(Theme::HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_MD))
                    .child(
                        div()
                            .id("settings-back")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(Theme::SPACE_SM))
                            .py(px(3.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                            .child(SharedString::from("←"))
                            .child(SharedString::from("Back to chats")),
                    ),
            )
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(SettingsSection::ALL.into_iter().map(|item| {
                        let selected = item == section;
                        popover::menu_row(theme, selected)
                            .id(SharedString::from(format!("settings-nav-{}", item.label())))
                            .text_size(px(13.0))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                            )
                            .child(SharedString::from(item.label()))
                    })),
            )
            .into_any_element()
    }

    /// One session row: title + status dot, click selects, right-click opens
    /// the Rename/Archive/Delete context menu.
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        indicator: Indicator,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dot_color = match indicator {
            Indicator::Working => Some(theme.accent),
            Indicator::AwaitingInput => Some(theme.warning),
            Indicator::Errored => Some(theme.danger),
            Indicator::None => None,
        };
        let (hover, active, text, muted) = (
            theme.element_hover,
            theme.element_active,
            theme.text,
            theme.text_muted,
        );
        let select_id = id.clone();
        let menu_id = id.clone();
        div()
            .id(SharedString::from(format!("chat-{id}")))
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .when_some(dot_color, |el, color| {
                el.child(div().size(px(6.0)).rounded_full().flex_none().bg(color))
            })
            .child(div().flex_1().truncate().child(title))
            .into_any_element()
    }

    /// Chat-mode sidebar: new-session + grouped toggle, the session list (flat
    /// or grouped by project), the notice strip, and the UserMenu (§1.6).
    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let now = Utc::now();
        let (chats, meta, user) = {
            let state = self.state.read(cx);
            let chats: Vec<comet_proto::Chat> = state.visible_chats().cloned().collect();
            let meta: std::collections::HashMap<String, (Indicator, bool)> = chats
                .iter()
                .map(|c| {
                    (
                        c.id.clone(),
                        (
                            state.indicator_for(&c.id, now),
                            state.selected_chat.as_deref() == Some(c.id.as_str()),
                        ),
                    )
                })
                .collect();
            (chats, meta, state.auth_user().cloned())
        };
        let grouped = self.settings.sidebar_grouped;

        let mut list_items: Vec<AnyElement> = Vec::new();
        let row_for = |shell: &Self, chat: &comet_proto::Chat, cx: &mut Context<Self>| {
            let (indicator, selected) = meta
                .get(&chat.id)
                .copied()
                .unwrap_or((Indicator::None, false));
            shell.render_chat_row(
                chat.id.clone(),
                chat.title
                    .clone()
                    .unwrap_or_else(|| "New session".into())
                    .into(),
                indicator,
                selected,
                theme,
                cx,
            )
        };
        if grouped {
            for group in group_chats(chats.iter()) {
                list_items.push(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .pt(px(Theme::SPACE_SM))
                        .pb(px(2.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(group.label.clone()))
                        .into_any_element(),
                );
                for chat in group.chats {
                    list_items.push(row_for(self, chat, cx));
                }
            }
        } else {
            for chat in &chats {
                list_items.push(row_for(self, chat, cx));
            }
        }

        let user_line: SharedString = user
            .as_ref()
            .map(|u| u.name.clone().unwrap_or_else(|| u.email.clone()).into())
            .unwrap_or_else(|| SharedString::from("Not signed in"));
        let user_email: Option<SharedString> = user.as_ref().map(|u| u.email.clone().into());
        let user_menu = self.render_user_menu(user_line.clone(), user_email.clone(), theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
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
                            .text_color(theme.text_muted)
                            .child(SharedString::from("This device")),
                    ),
            )
            // "New session" + grouped-by-project toggle.
            .child(
                div()
                    .mx(px(Theme::SPACE_MD))
                    .mb(px(Theme::SPACE_SM))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .id("new-session")
                            .flex_1()
                            .px(px(Theme::SPACE_MD))
                            .py(px(6.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .hover(|s| s.bg(Theme::dark().element_hover))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.route = Route::Chat;
                                this.state.update(cx, |s, cx| s.select_chat(None, cx));
                                cx.notify();
                            }))
                            .child(SharedString::from("New session")),
                    )
                    .child(
                        // Grouped-by-project toggle (persisted).
                        div()
                            .id("sidebar-group-toggle")
                            .px(px(Theme::SPACE_SM))
                            .py(px(6.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(if grouped {
                                theme.border_strong
                            } else {
                                theme.border
                            })
                            .text_size(px(12.0))
                            .text_color(if grouped {
                                theme.text
                            } else {
                                theme.text_faint
                            })
                            .when(grouped, |el| el.bg(theme.element_active))
                            .cursor_pointer()
                            .hover(|s| s.bg(Theme::dark().element_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_grouped(cx)))
                            .child(SharedString::from("⊟")),
                    ),
            )
            // Session list.
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
                    .children(list_items),
            )
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(user_menu)
            .into_any_element()
    }

    /// UserMenu (§1.6): name/email trigger row; menu with plan badge, Open
    /// settings, Sign out.
    fn render_user_menu(
        &mut self,
        user_line: SharedString,
        user_email: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu_open;
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .px(px(Theme::SPACE_MD))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .cursor_pointer()
            .hover(|s| s.bg(Theme::dark().element_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                // A click that just dismissed the menu (outside-click on the
                // trigger) must not instantly reopen it.
                let just_dismissed = this
                    .user_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.user_menu_open = !this.user_menu_open && !just_dismissed;
                this.user_menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .truncate()
                            .child(user_line.clone()),
                    )
                    .when_some(user_email.clone(), |el, email| {
                        el.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .truncate()
                                .child(email),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(5.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(9.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Alpha")),
            );
        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.user_menu_open = false;
                    this.user_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(user_line),
                        )
                        .when_some(user_email, |el, email| {
                            el.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_faint)
                                    .child(email),
                            )
                        })
                        .child(
                            div()
                                .pt(px(2.0))
                                .text_size(px(10.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Plan: Alpha")),
                        ),
                )
                .child(div().h(px(1.0)).mx(px(4.0)).my(px(2.0)).bg(theme.border))
                .child(
                    popover::menu_row(theme, false)
                        .id("user-menu-settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsSection::Devices, cx)
                        }))
                        .child(SharedString::from("Open settings")),
                )
                .child(
                    popover::menu_row(theme, false)
                        .id("user-menu-signout")
                        .text_color(theme.danger)
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(SharedString::from("Sign out")),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("user-menu-popover", menu));
        }
        trigger.into_any_element()
    }

    /// Floating layers owned by the shell: the session context menu and the
    /// rename / delete-confirm dialogs.
    fn render_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((chat_id, position)) = self.chat_menu.clone() {
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.chat_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false)
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(SharedString::from("Rename…")),
                )
                .child(
                    popover::menu_row(&theme, false)
                        .id("chat-menu-archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archive_chat(archive_id.clone(), cx)
                        }))
                        .child(SharedString::from("Archive")),
                )
                .child(
                    popover::menu_row(&theme, false)
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chat_menu = None;
                            this.delete_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(SharedString::from("Delete…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("chat-context-menu", position, menu));
        }

        if let Some(dialog) = &self.rename_dialog {
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
                        .child(SharedString::from("Rename session")),
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
                                .id("rename-chat-cancel")
                                .px(px(Theme::SPACE_MD))
                                .py(px(4.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(Theme::dark().element_hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                }))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            div()
                                .id("rename-chat-save")
                                .px(px(Theme::SPACE_MD))
                                .py(px(4.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .bg(theme.accent_strong)
                                .text_size(px(12.0))
                                .text_color(gpui::white())
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)))
                                .child(SharedString::from("Save")),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", card));
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
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
                        .child(SharedString::from("Delete this session?")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "The chat disappears from every device. The transcript doc is kept.",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(Theme::SPACE_SM))
                        .child(
                            div()
                                .id("delete-chat-cancel")
                                .px(px(Theme::SPACE_MD))
                                .py(px(4.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(Theme::dark().element_hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                }))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            div()
                                .id("delete-chat-confirm")
                                .px(px(Theme::SPACE_MD))
                                .py(px(4.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .bg(theme.danger)
                                .text_size(px(12.0))
                                .text_color(gpui::white())
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                }))
                                .child(SharedString::from("Delete")),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", card));
        }

        overlays
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

        // Settings route: header title "Settings" + the section outlet — no
        // composer/terminal/status strip (feature-inventory §1.3 header variants).
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
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
                        .child(header_button(
                            "toggle-sidebar",
                            "☰",
                            hover,
                            muted,
                            cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                        ))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.0))
                                .text_color(text)
                                .child(SharedString::from("Settings")),
                        ),
                )
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let title: SharedString = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "comet".into())
                .into()
        };
        let has_selection = self.state.read(cx).selected_chat.is_some();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark. The composer sits
        // below either (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(Theme::SPACE_MD))
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(Theme::SPACE_MD))
                        .child(
                            // Watermark.
                            div()
                                .text_size(px(28.0))
                                .text_color(theme.border_strong)
                                .child(SharedString::from("comet")),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(faint)
                                .child(SharedString::from("Send a message to start")),
                        ),
                ))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
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
                    .child(header_button(
                        "toggle-sidebar",
                        "☰",
                        hover,
                        muted,
                        cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(text)
                            .child(title),
                    )
                    .child(header_button(
                        "toggle-terminal",
                        "Terminal",
                        hover,
                        muted,
                        cx.listener(|this, _, _, cx| this.toggle_terminal(cx)),
                    ))
                    .child(header_button(
                        "toggle-changes",
                        "Changes",
                        hover,
                        muted,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                    )),
            )
            .child(div().flex_1().min_h_0().child(outlet))
            .child(self.render_terminal_container(cx))
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts.
            .child(status)
            .child(self.composer.clone())
            .into_any_element()
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target();
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Restore-on-boot: an open panel needs its entity (and set_open) even
        // if toggle_terminal never ran this session.
        if self.settings.terminal_open && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        let container = div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .child(inner);
        match tween {
            Some(WidthTween { from, to, epoch }) => container
                .with_animation(
                    ("terminal-height", epoch),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, to, t))),
                )
                .into_any_element(),
            None => container.h(px(target)).into_any_element(),
        }
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);
        let mode_line: SharedString = match state.engine().map(|e| e.mode()) {
            Some(EngineMode::InProcess) => "engine: in-process".into(),
            Some(EngineMode::Remote { url }) => format!("engine: {url}").into(),
            None => "".into(),
        };

        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .text_size(px(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip
                .text_color(theme.text_faint)
                .child(mode_line)
                .into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        let elapsed_secs = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .map(|t| now.signed_duration_since(t).num_seconds())
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        match indicator {
            Indicator::Working => {
                let word =
                    transcript::flavour_word(transcript::flavour_seed(&chat_id), elapsed_secs);
                strip
                    .child(loaders::gradient_spinner("working-indicator", &theme, 3.0))
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{word}…"))),
                    )
                    .child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(transcript::format_elapsed(elapsed_secs))),
                    )
                    .into_any_element()
            }
            Indicator::AwaitingInput => strip
                .text_color(theme.warning)
                .child(SharedString::from("Awaiting your input"))
                .into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .text_color(theme.text_muted)
                .child(SharedString::from("Sending…"))
                .into_any_element(),
            Indicator::None => strip
                .text_color(theme.text_faint)
                .child(mode_line)
                .into_any_element(),
        }
    }

    /// Right "Changes" pane — hidden by default, drag-resizable; content is the
    /// lazy [`Changes`] diff viewer (created on first open).
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let (bg, border) = (theme.surface, theme.border);
        let content: AnyElement = if self.settings.right_pane_open {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        let inner = div()
            .w(px(self.settings.right_pane_width))
            .h_full()
            .child(content);
        let target = self.right_target();
        self.pane_container(
            "right-pane-width",
            self.right_tween,
            target,
            div()
                .h_full()
                .bg(bg)
                .when(target > 0.0, |el| el.border_l_1().border_color(border))
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
                .child(
                    div()
                        .text_color(danger)
                        .child(SharedString::from("Backend unreachable")),
                )
                .child(
                    div()
                        .text_color(muted)
                        .child(SharedString::from(error.clone())),
                )
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
            // Sign-in card: kick the WorkOS browser flow (M4b); the AuthStatus
            // stream flips the gate once the loopback callback lands.
            _ => card
                .child(div().text_color(text).child(SharedString::from("Sign in")))
                .child(div().text_color(muted).child(SharedString::from(
                    "Sign in with your browser to connect this device.",
                )))
                .child(
                    div()
                        .id("sign-in")
                        .px(px(Theme::SPACE_MD))
                        .py(px(6.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .bg(Theme::dark().accent_strong)
                        .text_color(gpui::white())
                        .cursor_pointer()
                        .hover(move |s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Sign in with browser")),
                ),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(motion::dialog_in("gate-card", card))
            .into_any_element()
    }

    /// The OrgGate ("Create your workspace"): name form + existing memberships
    /// + "Use a different account" (feature-inventory §1.2).
    fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let submitting = org.submitting;
        let error = org.error.clone();
        let name_input = org.name_input.clone();
        let orgs = org.orgs.clone();

        let memberships: AnyElement = match &orgs {
            Loadable::Idle | Loadable::Loading => popover::skeleton_rows("org-skeleton", &theme, 2),
            Loadable::Error(message) => popover::error_row(&theme, message)
                .child(
                    div()
                        .id("orgs-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| this.load_orgs(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(rows) if rows.is_empty() => Empty.into_any_element(),
            Loadable::Ready(rows) => div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Or join an existing workspace")),
                )
                .children(rows.iter().enumerate().map(|(ix, row)| {
                    let org_id = row.organization_id.clone();
                    popover::menu_row(&theme, false)
                        .id(("org-row", ix))
                        .border_1()
                        .border_color(theme.border)
                        .when(submitting, |el| el.opacity(0.6))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_org(org_id.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from(row.name.clone())),
                        )
                }))
                .into_any_element(),
        };

        let card = div()
            .w(px(400.0))
            .p(px(Theme::SPACE_LG))
            .rounded(px(Theme::PANEL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .text_size(px(15.0))
                    .text_color(theme.text)
                    .child(SharedString::from("Create your workspace")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Chats and devices sync inside a workspace.",
                    )),
            )
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .child(name_input),
            )
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            })
            .child(
                div()
                    .id("create-org")
                    .px(px(Theme::SPACE_MD))
                    .py(px(6.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .bg(theme.accent_strong)
                    .text_size(px(13.0))
                    .text_color(gpui::white())
                    .when(submitting, |el| el.opacity(0.6))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(|this, _, _, cx| this.create_org(cx)))
                    .child(SharedString::from(if submitting {
                        "Creating…"
                    } else {
                        "Create workspace"
                    })),
            )
            .child(memberships)
            .child(
                div()
                    .id("org-signout")
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .cursor_pointer()
                    .hover(|s| s.text_color(Theme::dark().text_muted))
                    .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                    .child(SharedString::from("Use a different account")),
            );

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(motion::dialog_in("org-gate-card", card))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            .on_action(cx.listener(|this, _: &ToggleTerminal, _, cx| this.toggle_terminal(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| this.toggle_right_pane(cx)));

        let root = match &gate {
            GatePhase::Ready => {
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                let main_width = viewport - self.sidebar_target() - self.right_target() - 10.0;
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx)
                });

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
                let overlays = self.render_overlays(cx);
                root.child(sidebar)
                    .child(sidebar_handle)
                    .child(main)
                    .when(right_open, |el| el.child(right_handle))
                    .child(right)
                    .children(overlays)
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::OrgGate => {
                let card = self.render_org_gate(cx);
                root.child(card)
            }
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
