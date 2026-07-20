//! Settings → Agents / accounts (feature-inventory §1.9): provider cards
//! (Claude Code, Codex) with account rows — email, plan badge, Active, usage
//! meters (indigo → amber ≥80% → red ≥95%, reset time), Switch / Forget — plus
//! the add-account dialogs (paste-code and browser-poll flows), loading
//! skeletons, and a device switcher that retargets which device's logins are
//! shown (`targetDeviceId` passthrough).
//!
//! The accounts RPC surface is being implemented engine-side in parallel —
//! every call here surfaces failures as inline UI states rather than assuming
//! the methods exist.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, Hsla, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use std::time::Duration;

use comet_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessId,
};
use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// "resets in 2h 05m" / "resets in 12m" / "resets soon". Pure.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    let at = resets_at?;
    let mins = at.signed_duration_since(now).num_minutes();
    Some(if mins <= 0 {
        "resets soon".to_string()
    } else if mins < 60 {
        format!("resets in {mins}m")
    } else {
        format!("resets in {}h {:02}m", mins / 60, mins % 60)
    })
}

/// The provider cards, in display order.
pub const PROVIDERS: [(HarnessId, &str); 2] = [
    (HarnessId::ClaudeCode, "Claude Code"),
    (HarnessId::Codex, "Codex"),
];

/// Accounts of one provider, active first (stable otherwise). Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    let mut accounts: Vec<&AgentAccount> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect();
    accounts.sort_by_key(|a| !a.active);
    accounts
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting,
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

pub struct AccountsPage {
    state: Entity<AppState>,
    /// Which device's logins are shown; `None` = this device (no passthrough).
    target_device: Option<String>,
    device_menu_open: bool,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let code_input = cx.new(|cx| ComposerInput::new("Paste the code…", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let mut page = Self {
            state,
            target_device: None,
            device_menu_open: false,
            snapshot: Loadable::Idle,
            busy_account: None,
            login: None,
            error: None,
            code_input,
            load_task: None,
            action_task: None,
            poll_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        page.load(false, cx);
        page
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.target_device != target {
            self.target_device = target;
            self.login = None;
            self.poll_task = None;
            self.load(false, cx);
        }
        self.device_menu_open = false;
        cx.notify();
    }

    /// Switch / Forget an account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        // Tolerant param shape: both `id` and `accountId` plus the harness.
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => page.load(false, cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.login = Some(LoginFlow::Starting);
        self.error = None;
        let params = self.params(serde_json::json!({ "harness": harness }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        cx.open_url(&start.url);
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        page.error = Some(format!("Login failed to start: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id, "code": code }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(true, cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The browser-wait poll loop: PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::Browser { start, .. }) = &self.login else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id }));
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let Some(LoginFlow::Browser { message, error, .. }) = &mut page.login else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(true, cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
                Some(start.login_id.clone())
            }
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).engine().cloned()) {
            let params = self.params(serde_json::json!({ "loginId": login_id }));
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    // ---- render pieces ----

    fn render_usage_meter(
        &self,
        window: &comet_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let color = usage_color(usage_level(fraction), theme);
        let reset = format_reset(window.resets_at, now);
        div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(window.label.clone()))
                    .child(SharedString::from(match reset {
                        Some(reset) => format!("{}% · {reset}", (fraction * 100.0).round() as u32),
                        None => format!("{}%", (fraction * 100.0).round() as u32),
                    })),
            )
            .child(
                // Meter track + fill (threshold-colored).
                div()
                    .h(px(4.0))
                    .w_full()
                    .rounded(px(2.0))
                    .bg(theme.element_hover)
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction))
                            .rounded(px(2.0))
                            .bg(color),
                    ),
            )
            .into_any_element()
    }

    fn render_account_row(
        &self,
        account: &AgentAccount,
        ix: usize,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let switch_account = account.clone();
        let forget_account = account.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(Theme::SPACE_MD))
            .py(px(Theme::SPACE_SM))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .border_color(if account.active {
                theme.border_strong
            } else {
                theme.border
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(email),
                    )
                    .when_some(account.plan_label.clone(), |el, plan| {
                        el.child(
                            div()
                                .px(px(5.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(theme.border)
                                .text_size(px(9.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(plan)),
                        )
                    })
                    .when(account.active, |el| {
                        el.child(
                            div()
                                .px(px(5.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(theme.accent)
                                .text_size(px(9.0))
                                .text_color(theme.accent)
                                .child(SharedString::from("Active")),
                        )
                    })
                    .when(!account.active && account.switchable, |el| {
                        el.child(
                            div()
                                .id(("account-switch", ix))
                                .px(px(Theme::SPACE_SM))
                                .py(px(2.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_size(px(10.0))
                                .text_color(theme.text)
                                .when(is_busy, |el| el.opacity(0.5))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.account_action(
                                        methods::ACTIVATE_AGENT_ACCOUNT,
                                        &switch_account,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from("Switch")),
                        )
                    })
                    .child(
                        div()
                            .id(("account-forget", ix))
                            .px(px(Theme::SPACE_SM))
                            .py(px(2.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .when(is_busy, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover).text_color(theme.danger))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.account_action(
                                    methods::FORGET_AGENT_ACCOUNT,
                                    &forget_account,
                                    cx,
                                );
                            }))
                            .child(SharedString::from("Forget")),
                    ),
            )
            .children(
                account
                    .usage_windows
                    .iter()
                    .map(|window| self.render_usage_meter(window, theme, now)),
            )
            .into_any_element()
    }

    fn render_login_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let login = self.login.as_ref()?;
        let body: AnyElement = match login {
            LoginFlow::Starting => div()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Starting login…")),
                )
                .child(popover::skeleton_rows("login-starting", &theme, 2))
                .into_any_element(),
            LoginFlow::PasteCode {
                start,
                submitting,
                error,
            } => {
                let url: SharedString = start.url.clone().into();
                let open_url = start.url.clone();
                let submitting = *submitting;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "Sign in in your browser, then paste the code below.",
                            )),
                    )
                    .child(
                        div()
                            .id("login-open-url")
                            .px(px(Theme::SPACE_SM))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(11.0))
                            .text_color(theme.accent)
                            .truncate()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.open_url(&open_url);
                            }))
                            .child(url),
                    )
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .py(px(5.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .child(self.code_input.clone()),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .id("login-submit-code")
                            .px(px(Theme::SPACE_MD))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .bg(theme.accent_strong)
                            .text_size(px(12.0))
                            .text_color(gpui::white())
                            .when(submitting, |el| el.opacity(0.6))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx)))
                            .child(SharedString::from(if submitting {
                                "Verifying…"
                            } else {
                                "Continue"
                            })),
                    )
                    .into_any_element()
            }
            LoginFlow::Browser {
                start,
                message,
                error,
            } => {
                let url: SharedString = start.url.clone().into();
                let open_url = start.url.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "Finish signing in in your browser — waiting for it to land…",
                            )),
                    )
                    .child(
                        div()
                            .id("login-open-url-browser")
                            .px(px(Theme::SPACE_SM))
                            .py(px(4.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(11.0))
                            .text_color(theme.accent)
                            .truncate()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.open_url(&open_url);
                            }))
                            .child(url),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .child(crate::loaders::gradient_spinner("login-poll", &theme, 3.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(
                                        message
                                            .clone()
                                            .unwrap_or_else(|| SharedString::from("Waiting…")),
                                    ),
                            ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.danger)
                                .child(message),
                        )
                    })
                    .into_any_element()
            }
        };
        let card = div()
            .w(px(420.0))
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
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Add account")),
                    )
                    .child(
                        div()
                            .id("login-cancel")
                            .px(px(Theme::SPACE_SM))
                            .py(px(2.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx)))
                            .child(SharedString::from("Cancel")),
                    ),
            )
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", card))
    }

    /// Device switcher: retargets which device's CLI logins are listed.
    fn render_device_switcher(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let devices = self.state.read(cx).devices.clone();
        let current: SharedString = match &self.target_device {
            None => "This device".into(),
            Some(id) => devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.clone())
                .into(),
        };
        let open = self.device_menu_open;
        let mut chip = div()
            .id("accounts-device-switcher")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(Theme::SPACE_SM))
            .py(px(3.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.device_menu_open = !this.device_menu_open;
                cx.notify();
            }))
            .child(current)
            .child(
                div()
                    .text_color(theme.text_faint)
                    .child(SharedString::from("▾")),
            );
        if open {
            let menu = popover::popover_card(&theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, self.target_device.is_none())
                        .id("device-target-local")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_target_device(None, cx);
                        }))
                        .child(SharedString::from("This device")),
                )
                .children(devices.into_iter().enumerate().map(|(ix, device)| {
                    let selected = self.target_device.as_deref() == Some(device.id.as_str());
                    let id = device.id.clone();
                    popover::menu_row(&theme, selected)
                        .id(("device-target", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_target_device(Some(id.clone()), cx);
                        }))
                        .child(SharedString::from(device.name.clone()))
                }))
                .into_any_element();
            chip = chip.child(popover::anchored_menu("device-switcher-menu", menu));
        }
        chip.into_any_element()
    }
}

impl Render for AccountsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let switcher = self.render_device_switcher(cx);
        let dialog = self.render_login_dialog(cx);

        let cards: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => vec![
                popover::skeleton_rows("accounts-skeleton-claude", &theme, 3),
                popover::skeleton_rows("accounts-skeleton-codex", &theme, 3),
            ],
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    popover::error_row(&theme, &message)
                        .child(
                            div()
                                .id("accounts-retry")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(|this, _, _, cx| this.load(false, cx)))
                                .child(SharedString::from("Retry")),
                        )
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                PROVIDERS
                    .into_iter()
                    .map(|(harness, name)| {
                        let accounts = provider_accounts(&snapshot, harness);
                        let warning = snapshot
                            .warnings
                            .iter()
                            .find(|w| w.harness == harness)
                            .map(|w| w.message.clone());
                        let rows: Vec<AnyElement> = accounts
                            .iter()
                            .enumerate()
                            .map(|(ix, account)| {
                                self.render_account_row(account, ix, &theme, now, cx)
                            })
                            .collect();
                        let add_id: SharedString = format!("add-account-{name}").into();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_SM))
                            .p(px(Theme::SPACE_MD))
                            .rounded(px(Theme::PANEL_RADIUS))
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .text_color(theme.text)
                                            .child(SharedString::from(name)),
                                    )
                                    .child(
                                        div()
                                            .id(add_id)
                                            .px(px(Theme::SPACE_SM))
                                            .py(px(3.0))
                                            .rounded(px(Theme::CONTROL_RADIUS))
                                            .border_1()
                                            .border_color(theme.border)
                                            .text_size(px(11.0))
                                            .text_color(theme.text)
                                            .cursor_pointer()
                                            .hover(|s| s.bg(theme.element_hover))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.start_login(harness, cx);
                                            }))
                                            .child(SharedString::from("Add account")),
                                    ),
                            )
                            .when_some(warning, |el, warning| {
                                el.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.warning)
                                        .child(SharedString::from(warning)),
                                )
                            })
                            .when(rows.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from("No accounts detected")),
                                )
                            })
                            .children(rows)
                            .into_any_element()
                    })
                    .collect()
            }
        };

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .p(px(Theme::SPACE_LG))
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Agent accounts")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .child(switcher)
                            .child(
                                div()
                                    .id("accounts-refresh")
                                    .px(px(Theme::SPACE_SM))
                                    .py(px(3.0))
                                    .rounded(px(Theme::CONTROL_RADIUS))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.element_hover))
                                    .on_click(cx.listener(|this, _, _, cx| this.load(true, cx)))
                                    .child(SharedString::from("Refresh usage")),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div()
                        .id("accounts-action-error")
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
            .children(cards)
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn usage_thresholds_match_comet() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting() {
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        assert_eq!(
            format_reset(Some(now + TimeDelta::minutes(12)), now),
            Some("resets in 12m".into())
        );
        assert_eq!(
            format_reset(Some(now + TimeDelta::minutes(125)), now),
            Some("resets in 2h 05m".into())
        );
        assert_eq!(
            format_reset(Some(now - TimeDelta::minutes(1)), now),
            Some("resets soon".into())
        );
    }

    #[test]
    fn provider_grouping_puts_active_first() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["c2", "c1"], "active account leads");
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }
}
