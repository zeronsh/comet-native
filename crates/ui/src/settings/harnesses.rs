//! Settings → Agents: enable/disable harnesses (the t3code models-page
//! arrangement — one card row per agent with a trailing toggle).
//!
//! The state is PER-DEVICE and lives on the engine (`harness-prefs.json` in
//! its data dir): CLI installs are per-device, so enablement is too. The
//! page-header device switcher (the Accounts pattern) retargets both the
//! `ListHarnesses` probe and the `SetHarnessEnabled` writes at any registered
//! device over the relay-forwarded RPCs.
//!
//! Enablement follows DETECTION: every harness whose CLI probe passes is on
//! unless the user switched it off, so installing an agent is all it takes
//! for it to appear here and in the composer. A harness whose CLI is missing
//! on the target device renders dimmed with an install hint and is never
//! enabled (enabling an agent that can't run would only manufacture
//! NotInstalled errors at send time); an ENABLED agent can always be turned
//! OFF except the last one standing — the composer needs something to run.
//! Catalogs from engines predating the detection model can still stamp
//! enabled-but-uninstalled rows; the hint covers that too. The engine
//! enforces the same gates where the state lives, so a raced or stale toggle
//! self-corrects from the RPC reply.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};

use zeron_engine::registry::{HarnessDescriptor, descriptor_enabled};
use zeron_proto::HarnessId;
use zeron_rpc::methods;

use crate::pickers::visible_harnesses;
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// One-line blurb per agent (the t3code models page pairs every toggle row
/// with a description; the catalog descriptor doesn't carry one).
pub fn blurb(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Anthropic's coding agent, driven through the Claude Code CLI.",
        HarnessId::Codex => "OpenAI's coding agent, driven through the Codex CLI.",
        HarnessId::Cursor => "Cursor's coding agent, driven through the cursor-agent CLI.",
        HarnessId::Devin => "Cognition's Devin agent (devin CLI).",
        HarnessId::Grok => "xAI's Grok Build agent (grok CLI).",
        HarnessId::Hermes => "Nous Research's Hermes Agent (hermes CLI).",
        HarnessId::Pi => "The pi coding agent (pi CLI).",
        HarnessId::Opencode => "SST's opencode agent (opencode CLI).",
        HarnessId::Mock => "Scripted test harness.",
    }
}

/// The CLI named in the not-installed hint.
pub fn cli_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor-agent",
        HarnessId::Devin => "devin",
        HarnessId::Grok => "grok",
        HarnessId::Hermes => "hermes",
        HarnessId::Pi => "pi",
        HarnessId::Opencode => "opencode",
        HarnessId::Mock => "mock",
    }
}

pub struct HarnessesPage {
    state: Entity<AppState>,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    /// Which device's harnesses are shown/edited; `None` = this device (no
    /// passthrough). Retargeted by the page-header device switcher.
    target_device: Option<String>,
    device_menu_open: bool,
    /// Whether the menu was open when the trigger press began — the menu's
    /// `on_mouse_down_out` closes it on that same press, so by click time a
    /// plain toggle would reopen (the [`popover::Popup`] press note, for
    /// this page's bool-state menu).
    device_menu_pressed_open: bool,
    /// Last refused/failed toggle (engine guards), shown in the error strip.
    error: Option<String>,
    load_task: Option<Task<()>>,
    toggle_task: Option<Task<()>>,
}

impl HarnessesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            harnesses: Loadable::Idle,
            target_device: None,
            device_menu_open: false,
            device_menu_pressed_open: false,
            error: None,
            load_task: None,
            toggle_task: None,
        };
        page.load(cx);
        page
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn with_target(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// Retarget the page at another device: a different device is a different
    /// install/enablement world, so drop the rows and reload through it.
    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        self.error = None;
        self.harnesses = Loadable::Idle;
        self.load(cx);
        cx.notify();
    }

    /// `ListHarnesses` against the target device (installed probe + enabled
    /// set both come from where the CLIs actually live).
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        self.harnesses = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LIST_HARNESSES, params).await;
            this.update(cx, |page, cx| {
                page.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Flip one harness on the target device. The reply carries the device's
    /// fresh catalog, so the rows repaint from the authoritative state in one
    /// round trip; refusals (engine guards) land in the error strip.
    fn toggle(&mut self, harness: HarnessId, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({
            "harness": harness,
            "enabled": enabled,
        }));
        self.error = None;
        self.toggle_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_HARNESS_ENABLED, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => {
                        if let Ok(list) = serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                            page.harnesses = Loadable::Ready(list);
                        }
                        // The composer caches its catalog per space — poke
                        // every Pickers to re-fetch, or the rail keeps the
                        // old set until restart.
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(err) => page.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The page-header device switcher (the Accounts pattern): platform glyph
    /// · name · presence dot · sort glyph, opening a dropdown of every
    /// registered device.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.success;
        let open = self.device_menu_open;

        let mut trigger =
            div()
                .id("harnesses-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.device_menu_pressed_open = this.device_menu_open;
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    // A press that found the menu open closes it — never
                    // reopen on the same gesture.
                    let pressed_open = std::mem::take(&mut this.device_menu_pressed_open);
                    this.device_menu_open = !pressed_open && !this.device_menu_open;
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_local = is_local;
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("harnesses-device-row-{ix}"))
                        .id(("harnesses-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Local device = no passthrough (calls stay direct).
                            let target = (!pick_local).then(|| pick_id.clone());
                            this.set_target_device(target, cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text_muted.opacity(0.35))
                                    .child(SharedString::from("You")),
                            )
                        })
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .flex_none()
                                .bg(if is_local {
                                    emerald
                                } else {
                                    crate::theme::ink(0.2)
                                }),
                        )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("harnesses-device-menu", menu, None));
        }
        trigger.into_any_element()
    }

    fn rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = Theme::of(cx).clone();
        let Loadable::Ready(list) = &self.harnesses else {
            return Vec::new();
        };
        let descriptors = visible_harnesses(list);
        let enabled_count = descriptors.iter().filter(|d| descriptor_enabled(d)).count();
        descriptors
            .into_iter()
            .enumerate()
            .map(|(ix, descriptor)| {
                let harness = descriptor.id;
                let installed = descriptor.installed;
                let enabled = descriptor_enabled(&descriptor);
                // The one enabled harness left can't be switched off — the
                // composer needs something to run — but only when it could
                // actually run: an uninstalled last harness stays togglable
                // (its hint says to turn it off) and the composer handles the
                // resulting empty set (mirrors the engine guard).
                let last_enabled = enabled && enabled_count == 1 && installed;
                // Turning OFF never needs the CLI (a default-on agent the
                // user doesn't want must not be stuck on because it isn't
                // installed); turning ON still does.
                let interactive = !last_enabled && (enabled || installed);
                let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
                let mut meta: Vec<gpui::AnyElement> = vec![
                    div()
                        .child(SharedString::from(blurb(harness)))
                        .into_any_element(),
                ];
                if !installed {
                    meta.push(
                        div()
                            .text_color(theme.warning_muted.opacity(0.9))
                            .child(SharedString::from(if enabled {
                                format!(
                                    "{} CLI not installed — turn it off or install it",
                                    cli_name(harness)
                                )
                            } else {
                                format!("Install the {} CLI to enable", cli_name(harness))
                            }))
                            .into_any_element(),
                    );
                }
                // widgets::row_tile with the brand tint honored (the Claude
                // mark keeps its orange, like the picker rail).
                let tile = div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(icon_path)
                            .size(px(16.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    );
                widgets::card_row(&theme, ix == 0)
                    .id(("harness-row", ix))
                    .when(!installed, |el| el.opacity(0.55))
                    .child(tile)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, descriptor.name.clone()))
                            .child(widgets::meta_line(&theme, meta)),
                    )
                    .child(
                        widgets::toggle_switch(&theme, enabled)
                            .id(("harness-toggle", ix))
                            .when(interactive, |el| {
                                el.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle(harness, !enabled, cx);
                                    }))
                            }),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for HarnessesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body: gpui::AnyElement = match &self.harnesses {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "harnesses-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                div()
                    .child(widgets::error_strip(&theme, message))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("harnesses-retry")
                            .mt(px(8.0))
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.load(cx);
                                cx.notify();
                            }))
                            .child(SharedString::from("Retry")),
                    )
                    .into_any_element()
            }
            Loadable::Ready(_) => {
                let rows = self.rows(cx);
                widgets::section_card(&theme)
                    .children(rows)
                    .into_any_element()
            }
        };
        let error = self
            .error
            .clone()
            .map(|message| widgets::error_strip(&theme, message).into_any_element());
        let switcher = self.render_device_switcher(&theme, cx);

        div()
            .id("harnesses-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(widgets::page_header(&theme, "Agents", None))
                            .child(switcher),
                    )
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose which coding agents the composer offers. The setting is per \
                             device — switch devices in the header. Agents whose CLI isn't \
                             installed on a device can't be enabled there.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .children(error)
                    .child(body),
            )
    }
}
