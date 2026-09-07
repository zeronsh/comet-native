//! Settings → Encryption (RFC 0001 §4): the per-profile vault's explicit
//! states and the four actions a person can take — set up a vault (and save
//! the recovery kit), approve this device from another one, approve other
//! devices by comparing their code, and remove a device (rotating keys).
//!
//! Everything here is a thin view over the engine's `Vault*` RPCs; no key
//! material ever reaches the UI except the recovery kit text, which is shown
//! once for the user to save and never persisted by the UI.

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, IntoElement, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use serde_json::Value;

use zeron_proto::WorkspaceScope;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Which text prompt is open (the page has one input at a time).
enum Prompt {
    /// Recovery: the user types the kit text.
    Recover,
}

struct PromptDialog {
    kind: Prompt,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct EncryptionPage {
    state: Entity<AppState>,
    status: Loadable<Value>,
    pending: Vec<Value>,
    /// Shown once after setup: (kit text, recovery file JSON).
    kit: Option<(String, String)>,
    kit_copied: bool,
    prompt: Option<PromptDialog>,
    error: Option<String>,
    busy: bool,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _observe: Subscription,
}

/// Human copy for each vault phase (RFC §4.3 state table).
pub fn phase_copy(status: &Value) -> (&'static str, String) {
    let phase = status.get("phase").and_then(Value::as_str).unwrap_or("");
    let reason = status
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match phase {
        "ready" => ("Encrypted", "Synced content is encrypted on your devices. Only approved devices, or someone with your recovery key, can read it.".into()),
        "notEnrolled" => {
            if status.get("remoteVault").and_then(Value::as_bool) == Some(true) {
                ("Approve this device", "This account already has an encrypted vault. Approve this device from another device, or use your recovery key.".into())
            } else {
                ("Not set up", "Sessions, files and workspace details are sent to the sync backend in the clear until you set up encryption.".into())
            }
        }
        "pending" => ("Waiting for approval", "Open Settings → Encryption on an approved device and compare the code below before approving.".into()),
        "locked" => ("Unlock this device", format!("Secure key storage is unavailable: {reason}")),
        "keyUpdateRequired" => ("Waiting for encryption keys", "Another device changed the vault's keys; sync resumes once the new keys arrive.".into()),
        "verificationFailed" => ("Sync paused", format!("Data could not be verified: {reason}")),
        "revoked" => ("Removed", "This device was removed from the vault. Approve it again from another device to resume.".into()),
        "unavailable" => ("Not available", reason),
        _ => ("Unknown", String::new()),
    }
}

impl EncryptionPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            status: Loadable::Idle,
            pending: Vec::new(),
            kit: None,
            kit_copied: false,
            prompt: None,
            error: None,
            busy: false,
            load_task: None,
            action_task: None,
            copy_task: None,
            _observe: observe,
        };
        page.load(cx);
        page
    }

    /// `VaultRefresh` (network reconcile + status) and, when this device is
    /// an approved member, the pending enrollment requests.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if matches!(self.status, Loadable::Idle) {
            self.status = Loadable::Loading;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let status = engine
                .client()
                .call(methods::VAULT_REFRESH, serde_json::json!({}))
                .await;
            let ready = status
                .as_ref()
                .ok()
                .and_then(|s| s.get("phase").and_then(Value::as_str))
                == Some("ready");
            let pending = if ready {
                engine
                    .client()
                    .call(methods::VAULT_PENDING_REQUESTS, serde_json::json!({}))
                    .await
                    .ok()
                    .and_then(|v| v.get("requests").and_then(Value::as_array).cloned())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            this.update(cx, |page, cx| {
                page.status = match status {
                    Ok(value) => Loadable::Ready(value),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                page.pending = pending;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Run one vault action, then reload. `after` receives the reply.
    fn action(
        &mut self,
        method: &'static str,
        params: Value,
        after: impl FnOnce(&mut Self, Value) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.busy = true;
        cx.notify();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(value) => after(page, value),
                    Err(err) => page.error = Some(err.to_string()),
                }
                page.load(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn setup(&mut self, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_SETUP,
            serde_json::json!({}),
            |page, value| {
                let kit = value
                    .get("kit")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let file = value
                    .get("recoveryFile")
                    .map(|f| serde_json::to_string_pretty(f).unwrap_or_default())
                    .unwrap_or_default();
                page.kit = Some((kit, file));
                page.kit_copied = false;
            },
            cx,
        );
    }

    fn request_enrollment(&mut self, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_REQUEST_ENROLLMENT,
            serde_json::json!({}),
            |_, _| {},
            cx,
        );
    }

    fn cancel_enrollment(&mut self, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_CANCEL_ENROLLMENT,
            serde_json::json!({}),
            |_, _| {},
            cx,
        );
    }

    fn approve(&mut self, request_id: String, code: String, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_APPROVE,
            serde_json::json!({ "requestId": request_id, "code": code }),
            |_, _| {},
            cx,
        );
    }

    fn reject(&mut self, request_id: String, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_REJECT,
            serde_json::json!({ "requestId": request_id }),
            |_, _| {},
            cx,
        );
    }

    fn revoke(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.action(
            methods::VAULT_REVOKE,
            serde_json::json!({ "deviceId": device_id }),
            |_, _| {},
            cx,
        );
    }

    fn open_recover(&mut self, cx: &mut Context<Self>) {
        let input = cx.new(|cx| ComposerInput::new("Recovery key (XXXXX-XXXXX-…)", cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_prompt(cx);
            }
        });
        self.prompt = Some(PromptDialog {
            kind: Prompt::Recover,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.prompt.take() else {
            return;
        };
        let text = dialog.input.read(cx).text().trim().to_string();
        if text.is_empty() {
            cx.notify();
            return;
        }
        match dialog.kind {
            Prompt::Recover => self.action(
                methods::VAULT_RECOVER,
                serde_json::json!({ "kit": text }),
                |_, _| {},
                cx,
            ),
        }
    }

    fn copy_kit(&mut self, cx: &mut Context<Self>) {
        let Some((kit, file)) = self.kit.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(format!(
            "Zeron recovery key: {kit}\n\nRecovery file:\n{file}\n"
        )));
        self.kit_copied = true;
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.kit_copied = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_prompt(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.prompt.as_ref()?;
        let input = dialog.input.clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Use recovery key"))
            .child(
                div()
                    .mt(px(8.0))
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Enter the recovery key you saved when you set up encryption. This \
                         adds this device under a fresh key epoch; other devices catch up \
                         automatically.",
                    )),
            )
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
                        popover::btn_ghost(&theme, "Cancel", "vault-prompt-cancel")
                            .id("vault-prompt-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.prompt = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Recover")
                            .id("vault-prompt-submit")
                            .on_click(cx.listener(|this, _, _, cx| this.submit_prompt(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("vault-prompt-dialog", viewport, card))
    }

    fn action_button(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let busy = self.busy;
        let button = if primary {
            popover::btn_primary(theme, label)
        } else {
            popover::btn_ghost(theme, label, id)
        };
        button
            .id(id)
            .when(busy, |el| el.opacity(0.5))
            .when(!busy, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            })
            .into_any_element()
    }

    fn render_status(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Loadable::Ready(status) = &self.status else {
            return widgets::section_card(theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "vault-skeleton",
                    theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element();
        };
        let status = status.clone();
        let phase = status
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let (title, copy) = phase_copy(&status);
        let epoch = status.get("epoch").and_then(Value::as_u64);
        let protection = status
            .get("protection")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut meta: Vec<AnyElement> =
            vec![div().child(SharedString::from(copy)).into_any_element()];
        if let Some(epoch) = epoch {
            meta.push(
                div()
                    .child(SharedString::from(format!(
                        "Key epoch {epoch} · keys protected by {}",
                        match protection.as_str() {
                            "keychain" => "the macOS Keychain",
                            "systemdCredential" => "a systemd credential (unattended)",
                            "keyFile" => "an operator key file (unattended)",
                            _ => "this process only",
                        }
                    )))
                    .into_any_element(),
            );
        }
        let mut actions: Vec<AnyElement> = Vec::new();
        match phase.as_str() {
            "notEnrolled" => {
                if status.get("remoteVault").and_then(Value::as_bool) == Some(true) {
                    actions.push(self.action_button(
                        theme,
                        "vault-request",
                        "Approve from another device",
                        true,
                        cx,
                        |this, cx| this.request_enrollment(cx),
                    ));
                    actions.push(self.action_button(
                        theme,
                        "vault-recover",
                        "Use recovery key",
                        false,
                        cx,
                        |this, cx| this.open_recover(cx),
                    ));
                } else {
                    actions.push(self.action_button(
                        theme,
                        "vault-setup",
                        "Set up encryption",
                        true,
                        cx,
                        |this, cx| this.setup(cx),
                    ));
                }
            }
            "pending" => {
                actions.push(self.action_button(
                    theme,
                    "vault-cancel",
                    "Cancel request",
                    false,
                    cx,
                    |this, cx| this.cancel_enrollment(cx),
                ));
            }
            "revoked" => {
                actions.push(self.action_button(
                    theme,
                    "vault-request-again",
                    "Approve from another device",
                    true,
                    cx,
                    |this, cx| this.request_enrollment(cx),
                ));
            }
            _ => {}
        }
        actions.push(self.action_button(
            theme,
            "vault-refresh",
            "Refresh",
            false,
            cx,
            |this, cx| this.load(cx),
        ));
        let badge = match phase.as_str() {
            "ready" => widgets::badge_active(theme, "On"),
            _ => widgets::badge(theme, title),
        };
        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .child(widgets::row_tile(theme, crate::icons::KEY_MINIMALISTIC))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(theme, "End-to-end encryption"))
                        .child(widgets::meta_line(theme, meta)),
                )
                .child(badge),
        );
        if phase == "pending"
            && let Some(code) = status.get("pairingCode").and_then(Value::as_str)
        {
            card = card.child(
                widgets::card_row(theme, false).child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(widgets::row_title(theme, "Comparison code"))
                        .child(
                            div()
                                .font_family("Geist Mono")
                                .text_size(crate::typography::ui_rems(22.0))
                                .text_color(theme.text)
                                .child(SharedString::from(code.to_string())),
                        )
                        .child(widgets::meta_line(
                            theme,
                            vec![div()
                                .child(SharedString::from(
                                    "Approve only if the approving device shows exactly this code.",
                                ))
                                .into_any_element()],
                        )),
                ),
            );
        }
        card = card.child(
            widgets::card_row(theme, false).child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(8.0))
                    .children(actions),
            ),
        );
        card.into_any_element()
    }

    fn render_kit(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kit, _) = self.kit.clone()?;
        let copied = self.kit_copied;
        Some(
            widgets::section_card(theme)
                .child(
                    widgets::card_row(theme, true).child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(widgets::row_title(theme, "Save your recovery key now"))
                            .child(
                                div()
                                    .font_family("Geist Mono")
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(kit)),
                            )
                            .child(widgets::warning_strip(
                                theme,
                                "If you lose every approved device and your recovery key, we \
                                 cannot recover your encrypted data. Resetting your account \
                                 password will not restore access.",
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(8.0))
                                    .child(
                                        popover::btn_primary(
                                            theme,
                                            if copied {
                                                "Copied"
                                            } else {
                                                "Copy key and file"
                                            },
                                        )
                                        .id("vault-kit-copy")
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| this.copy_kit(cx))),
                                    )
                                    .child(
                                        popover::btn_ghost(theme, "I saved it", "vault-kit-done")
                                            .id("vault-kit-done")
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.kit = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_pending(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = self
            .pending
            .iter()
            .enumerate()
            .map(|(ix, request)| {
                let request_id = request
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let code = request
                    .get("pairingCode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let device = request
                    .get("deviceId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let approve_id = request_id.clone();
                let approve_code = code.clone();
                let reject_id = request_id.clone();
                widgets::card_row(theme, ix == 0)
                    .id(("vault-pending", ix))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                format!("Device {}", crate::settings::devices::short_id(&device)),
                            ))
                            .child(
                                div()
                                    .font_family("Geist Mono")
                                    .text_size(crate::typography::ui_rems(18.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(code.clone())),
                            )
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Compare with the code on the new device. An approved \
                                         device can read all synced content and manage devices.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(self.action_button(
                        theme,
                        "vault-reject",
                        "Reject",
                        false,
                        cx,
                        move |this, cx| this.reject(reject_id.clone(), cx),
                    ))
                    .child(self.action_button(
                        theme,
                        "vault-approve",
                        "Codes match — approve",
                        true,
                        cx,
                        move |this, cx| this.approve(approve_id.clone(), approve_code.clone(), cx),
                    ))
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(widgets::field_label(theme, "Devices waiting for approval"))
                .child(widgets::section_card(theme).children(rows))
                .into_any_element(),
        )
    }

    fn render_devices(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Loadable::Ready(status) = &self.status else {
            return None;
        };
        let devices = status.get("devices")?.as_array()?.clone();
        if devices.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = devices
            .iter()
            .enumerate()
            .map(|(ix, device)| {
                let id = device
                    .get("deviceId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let active = device.get("status").and_then(Value::as_str) == Some("active");
                let this_device = device.get("thisDevice").and_then(Value::as_bool) == Some(true);
                let revoke_id = id.clone();
                let mut row = widgets::card_row(theme, ix == 0)
                    .id(("vault-device", ix))
                    .when(!active, |el| el.opacity(0.55))
                    .child(widgets::row_tile(theme, crate::icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                format!("Vault member {}", crate::settings::devices::short_id(&id)),
                            ))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    div()
                                        .child(SharedString::from(if active {
                                            "Approved"
                                        } else {
                                            "Removed"
                                        }))
                                        .into_any_element(),
                                ],
                            )),
                    );
                if this_device {
                    row = row.child(widgets::badge_active(theme, "This device"));
                } else if active {
                    row = row.child(self.action_button(
                        theme,
                        "vault-revoke",
                        "Remove",
                        false,
                        cx,
                        move |this, cx| this.revoke(revoke_id.clone(), cx),
                    ));
                }
                row.into_any_element()
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(widgets::field_label(theme, "Approved devices"))
                .child(widgets::section_card(theme).children(rows))
                .child(widgets::meta_line(
                    theme,
                    vec![
                        div()
                            .child(SharedString::from(
                                "Removing a device stops its future sync access after the change \
                             takes effect. It cannot erase information the device already \
                             downloaded.",
                            ))
                            .into_any_element(),
                    ],
                ))
                .into_any_element(),
        )
    }
}

impl Render for EncryptionPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let scope = self.state.read(cx).workspace_scope;
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());
        let load_error = match &self.status {
            Loadable::Error(message) => Some(message.clone()),
            _ => None,
        };
        let subtitle = match scope {
            Some(WorkspaceScope::Local) => {
                "This workspace is local-only; nothing is sent to a sync backend.".to_string()
            }
            _ => "Sessions, files and workspace details are encrypted before they reach our \
                  servers. Approved devices can read them; the sync backend stores ciphertext."
                .to_string(),
        };
        let status = self.render_status(&theme, cx);
        let kit = self.render_kit(&theme, cx);
        let pending = self.render_pending(&theme, cx);
        let devices = self.render_devices(&theme, cx);
        let dialog = self.render_prompt(window.viewport_size(), cx);

        div()
            .id("encryption-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Encryption", None))
                    .child(
                        widgets::page_subtitle(&theme, subtitle)
                            .max_w(px(512.0))
                            .line_height(px(20.0)),
                    )
                    .children(error)
                    .when_some(load_error, |el, message| {
                        el.child(widgets::error_strip(&theme, message))
                    })
                    .children(kit)
                    .child(status)
                    .children(pending)
                    .children(devices),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_copy_never_promises_encryption_before_ready() {
        let not_enrolled = serde_json::json!({ "phase": "notEnrolled", "remoteVault": false });
        assert_eq!(phase_copy(&not_enrolled).0, "Not set up");
        let existing = serde_json::json!({ "phase": "notEnrolled", "remoteVault": true });
        assert_eq!(phase_copy(&existing).0, "Approve this device");
        let locked = serde_json::json!({ "phase": "locked", "reason": "no keychain" });
        assert!(phase_copy(&locked).1.contains("no keychain"));
        assert_eq!(
            phase_copy(&serde_json::json!({ "phase": "ready" })).0,
            "Encrypted"
        );
        assert_eq!(
            phase_copy(&serde_json::json!({ "phase": "keyUpdateRequired" })).0,
            "Waiting for encryption keys"
        );
    }
}
