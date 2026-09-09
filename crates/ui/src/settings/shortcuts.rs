//! Settings → Shortcuts (feature-inventory §1.4): a table of the rebindable
//! bindings — click a combo to record (Esc cancels), live conflict detection,
//! per-row Reset and Restore defaults. Changes emit [`ShortcutsEvent`]; the
//! shell persists them and re-applies the app keymap.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::settings::{
    ComposerSendBehavior, KeymapConfig, ShortcutId, combo_from_keystroke, display_combo,
};
use crate::state::AppState;
use crate::theme::Theme;

/// Outcome of one keystroke while recording. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Esc — abandon recording, keep the old combo.
    Cancelled,
    /// A bare modifier (or unusable key) — stay recording.
    Ignored,
    /// A full combo landed.
    Set(String),
}

pub fn record_key(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> RecordOutcome {
    if key.eq_ignore_ascii_case("escape") {
        return RecordOutcome::Cancelled;
    }
    match combo_from_keystroke(ctrl, alt, shift, cmd, key) {
        Some(combo) => RecordOutcome::Set(combo),
        None => RecordOutcome::Ignored,
    }
}

#[derive(Debug, Clone)]
pub enum ShortcutsEvent {
    /// The keymap changed — persist + re-apply.
    KeymapChanged(KeymapConfig),
    /// The composer send behavior changed — persist + re-apply.
    ComposerSendBehaviorChanged(ComposerSendBehavior),
}

pub struct ShortcutsPage {
    /// Working copy (kept in sync with the shell via change events).
    keymap: KeymapConfig,
    composer_send_behavior: ComposerSendBehavior,
    recording: Option<ShortcutId>,
    /// A rejected record attempt ("{Combo} is already assigned to {label}.") —
    /// conflicts never persist; they're refused at record time, as in zeron.
    conflict_notice: Option<SharedString>,
    focus: FocusHandle,
    // The page never talks RPC; state is kept for parity with sibling pages
    // (and future per-device keymaps).
    _state: Entity<AppState>,
}

impl EventEmitter<ShortcutsEvent> for ShortcutsPage {}

impl ShortcutsPage {
    pub fn new(
        state: Entity<AppState>,
        keymap: KeymapConfig,
        composer_send_behavior: ComposerSendBehavior,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            keymap,
            composer_send_behavior,
            recording: None,
            conflict_notice: None,
            focus: cx.focus_handle(),
            _state: state,
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::KeymapChanged(self.keymap.clone()));
        cx.notify();
    }

    fn set_composer_send_behavior(
        &mut self,
        behavior: ComposerSendBehavior,
        cx: &mut Context<Self>,
    ) {
        if behavior == ComposerSendBehavior::ModEnter
            && let Some(owner) = modifier_send_conflict_owner(&self.keymap)
        {
            self.conflict_notice = Some(
                format!(
                    "{} is already assigned to {}. Change that shortcut first.",
                    display_combo("mod-enter"),
                    owner.label()
                )
                .into(),
            );
            cx.notify();
            return;
        }
        if self.composer_send_behavior != behavior {
            self.composer_send_behavior = behavior;
            self.conflict_notice = None;
            cx.emit(ShortcutsEvent::ComposerSendBehaviorChanged(behavior));
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(recording) = self.recording else {
            return;
        };
        let mods = &event.keystroke.modifiers;
        match record_key(
            &event.keystroke.key,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        ) {
            RecordOutcome::Cancelled => {
                self.recording = None;
                cx.notify();
            }
            RecordOutcome::Ignored => {}
            RecordOutcome::Set(combo) => {
                if send_combo_is_reserved(self.composer_send_behavior, &combo) {
                    self.conflict_notice = Some(
                        format!(
                            "{} is already assigned to Send message.",
                            display_combo(&combo)
                        )
                        .into(),
                    );
                    self.recording = None;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                // A combo already bound elsewhere is REFUSED, naming the owner
                // (zeron settings.shortcuts.tsx: "… is already assigned to …").
                if let Some(owner) = conflict_owner(&self.keymap, recording, &combo) {
                    self.conflict_notice = Some(
                        format!(
                            "{} is already assigned to {}.",
                            display_combo(&combo),
                            owner.label()
                        )
                        .into(),
                    );
                    self.recording = None;
                    cx.notify();
                } else {
                    self.keymap.set(recording, combo);
                    self.recording = None;
                    self.conflict_notice = None;
                    self.commit(cx);
                }
            }
        }
        cx.stop_propagation();
    }

    /// One shortcut row: label + description left, Reset when customized, and
    /// the click-to-record combo chip (recording inverts it to
    /// white-on-black). `ix` is the id's position in [`ShortcutId::ALL`]
    /// (unique element ids across the group cards); `gx` is the row's place
    /// in its own card (separator rule).
    fn render_row(
        &self,
        id: ShortcutId,
        ix: usize,
        gx: usize,
        recording: Option<ShortcutId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let combo = self.keymap.get(id).to_string();
        let is_recording = recording == Some(id);
        let non_default = combo != id.default_combo();
        let chip_text: SharedString = if is_recording {
            "Press keys…".into()
        } else {
            display_combo(&combo).into()
        };
        // zeron settings.shortcuts.tsx row: min-h-[72px] px-5 gap-5.
        div()
            .min_h(px(72.0))
            .px(px(20.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(20.0))
            .when(gx > 0, |el| el.border_t_1().border_color(theme.border))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(id.label())),
                    )
                    .child(
                        div()
                            .mt(px(2.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(description(id))),
                    ),
            )
            .when(non_default && !is_recording, |el| {
                el.child(
                    div()
                        .id(("shortcut-reset", ix))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keymap.reset(id);
                            this.recording = None;
                            this.commit(cx);
                        }))
                        .child(SharedString::from("Reset")),
                )
            })
            .child(
                div()
                    .id(("shortcut-combo", ix))
                    .min_w(px(96.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .flex()
                    .justify_center()
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(12.0))
                    .cursor_pointer()
                    .map(|el| {
                        if is_recording {
                            el.border_color(theme.text.opacity(0.3))
                                .bg(theme.text)
                                .text_color(theme.on_solid)
                        } else {
                            el.border_color(theme.border)
                                .bg(theme.bg)
                                .text_color(theme.text)
                                .hover(|s| {
                                    // `hover:border-foreground/20` — the
                                    // neutral foreground, not pure white.
                                    s.border_color(theme.text.opacity(0.2))
                                        .bg(crate::theme::ink(0.03))
                                })
                        }
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.recording = Some(id);
                        this.conflict_notice = None;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }))
                    .child(chip_text),
            )
    }
}

/// The shortcut (other than `id`) already bound to `combo`, if any. Pure.
pub fn conflict_owner(keymap: &KeymapConfig, id: ShortcutId, combo: &str) -> Option<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .find(|&other| other != id && keymap.get(other) == combo)
}

pub fn modifier_send_conflict_owner(keymap: &KeymapConfig) -> Option<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .find(|&id| keymap.get(id) == "mod-enter")
}

pub fn send_combo_is_reserved(behavior: ComposerSendBehavior, combo: &str) -> bool {
    behavior == ComposerSendBehavior::ModEnter && combo == "mod-enter"
}

pub fn modifier_send_label(is_macos: bool) -> &'static str {
    if is_macos { "⌘ Enter" } else { "Ctrl Enter" }
}

/// The page's sections, in display order. [`group`] is a total match, so every
/// [`ShortcutId::ALL`] entry lands in exactly one — a shortcut added later
/// extends the match and appears on the page by construction
/// (`every_shortcut_lands_in_a_rendered_group` holds the other half: its group
/// name must be listed here).
const GROUP_ORDER: [&str; 5] = ["Files", "Browser", "Panels", "Sessions", "Jump to session"];

/// The section a shortcut's row renders under.
fn group(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::SaveFile => "Files",
        ShortcutId::BrowserReload => "Browser",
        ShortcutId::ToggleSidebar | ShortcutId::ToggleChanges | ShortcutId::ToggleTerminal => {
            "Panels"
        }
        ShortcutId::NewSession
        | ShortcutId::NextSession
        | ShortcutId::PrevSession
        | ShortcutId::ArchiveSession => "Sessions",
        ShortcutId::JumpSession(_) => "Jump to session",
    }
}

/// One-line purpose copy per shortcut (zeron lib/shortcuts.ts
/// `SHORTCUT_DEFINITIONS` descriptions, verbatim).
fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::SaveFile => "Save the active workspace file.",
        ShortcutId::BrowserReload => "Reload the focused browser tab.",
        ShortcutId::ToggleSidebar => "Show or hide sessions and settings navigation.",
        ShortcutId::ToggleChanges => "Show or hide the right sidebar for the current session.",
        ShortcutId::ToggleTerminal => "Show or hide the terminal for the current session.",
        ShortcutId::NewSession => "Open a blank session canvas to start a new session.",
        ShortcutId::NextSession => "Select the next session in the sidebar, wrapping at the end.",
        ShortcutId::PrevSession => {
            "Select the previous session in the sidebar, wrapping at the start."
        }
        ShortcutId::ArchiveSession => "Move the current session to the archived shelf.",
        // One line per slot would repeat itself nine times; the ordinal is
        // already in the row's label.
        ShortcutId::JumpSession(_) => "Open the session at this place in the sidebar list.",
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let recording = self.recording;
        let send_behavior = self.composer_send_behavior;
        let customized = self.keymap != KeymapConfig::default()
            || send_behavior != ComposerSendBehavior::default();
        let modifier_label = modifier_send_label(cfg!(target_os = "macos"));

        let send_behavior_control = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .when(send_behavior != ComposerSendBehavior::Enter, |el| {
                el.child(
                    div()
                        .id("composer-send-reset")
                        .size(px(26.0))
                        .rounded(px(7.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ink(0.04)).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_composer_send_behavior(ComposerSendBehavior::Enter, cx)
                        }))
                        .child(
                            crate::icons::icon(crate::icons::RESTART)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                )
            })
            .child(
                div()
                    .id("composer-send-behavior")
                    .flex()
                    .flex_row()
                    .rounded(px(9.0))
                    .p(px(2.0))
                    .bg(crate::theme::ink(0.04))
                    .children(
                        [
                            (ComposerSendBehavior::Enter, "Enter"),
                            (ComposerSendBehavior::ModEnter, modifier_label),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (behavior, label))| {
                            let selected = send_behavior == behavior;
                            div()
                                .id(("composer-send-option", ix))
                                .min_w(px(72.0))
                                .px(px(12.0))
                                .py(px(6.0))
                                .rounded(px(7.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(12.0))
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .when(selected, |el| {
                                    el.bg(theme.bg)
                                        .border_1()
                                        .border_color(theme.border.opacity(0.8))
                                })
                                .when(!selected, |el| {
                                    el.cursor_pointer()
                                        .hover(|s| s.text_color(theme.text))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_composer_send_behavior(behavior, cx)
                                        }))
                                })
                                .child(SharedString::from(label))
                        }),
                    ),
            );

        let send_behavior_row = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .min_h(px(84.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Send messages with"))
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .max_w(px(430.0))
                                    .text_size(px(11.5))
                                    .line_height(px(17.0))
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from(
                                        "Choose whether Enter sends immediately or starts a new paragraph. Shift+Enter always inserts a line break.",
                                    )),
                            ),
                    )
                    .child(send_behavior_control),
            );

        // One card per group, each under its small section label — the flat
        // 16-row table read as one undifferentiated wall. `ix` (the id's
        // position in ALL) keys the interactive elements, so ids stay unique
        // across cards.
        let mut groups: Vec<gpui::AnyElement> = Vec::new();
        for name in GROUP_ORDER {
            let mut card = widgets::section_card(&theme);
            let ids = ShortcutId::ALL.into_iter().filter(|&id| group(id) == name);
            for (gx, id) in ids.enumerate() {
                let ix = ShortcutId::ALL.iter().position(|&a| a == id).unwrap_or(0);
                card = card.child(self.render_row(id, ix, gx, recording, &theme, cx));
            }
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(widgets::field_label(&theme, name))
                    .child(card)
                    .into_any_element(),
            );
        }

        // Helper line stays in the muted tone even for a rejected conflict —
        // the message names the specific clash (zeron settings.shortcuts.tsx).
        let helper: SharedString = if recording.is_some() {
            "Press Escape to cancel.".into()
        } else if let Some(notice) = self.conflict_notice.clone() {
            notice
        } else {
            "Shortcuts must be unique.".into()
        };

        div()
            .id("shortcuts-page")
            .size_full()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key_down(event, cx)),
            )
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(widgets::page_header(&theme, "Keyboard shortcuts", None))
                                    .child(
                                        widgets::page_subtitle(
                                            &theme,
                                            "Click a binding, then press the key combination you \
                                             want to use. Changes apply immediately and stay on \
                                             this device.",
                                        )
                                        .max_w(px(512.0))
                                        .line_height(px(20.0)),
                                    ),
                            )
                            .child({
                                // `disabled:opacity-35` when nothing is
                                // customized or while recording.
                                let disabled = !customized || recording.is_some();
                                widgets::ghost_action(&theme)
                                    .id("shortcuts-restore-defaults")
                                    .flex_none()
                                    .when(disabled, |el| el.opacity(0.35))
                                    .when(!disabled, |el| {
                                        el.hover(|s| {
                                            s.bg(crate::theme::ink(0.04)).text_color(theme.text)
                                        })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.keymap = KeymapConfig::default();
                                                this.recording = None;
                                                this.conflict_notice = None;
                                                this.commit(cx);
                                                this.set_composer_send_behavior(
                                                    ComposerSendBehavior::Enter,
                                                    cx,
                                                );
                                            }),
                                        )
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::RESTART)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Restore defaults"))
                            }),
                    )
                    .child(send_behavior_row.mt(px(32.0)))
                    .child(
                        div()
                            .mt(px(28.0))
                            .flex()
                            .flex_col()
                            .gap(px(28.0))
                            .children(groups),
                    )
                    .child(
                        div()
                            .mt(px(12.0))
                            .px(px(4.0))
                            .min_h(px(20.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(helper),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_outcomes() {
        assert_eq!(
            record_key("escape", false, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("Escape", true, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("s", false, false, false, true),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        // macOS-only: elsewhere ctrl IS the primary and records as "mod".
        #[cfg(target_os = "macos")]
        assert_eq!(
            record_key("tab", true, false, true, false),
            RecordOutcome::Set("ctrl-shift-tab".into())
        );
        // Bare modifiers stay recording.
        assert_eq!(
            record_key("shift", false, false, true, false),
            RecordOutcome::Ignored
        );
        assert_eq!(
            record_key("ctrl", true, false, false, false),
            RecordOutcome::Ignored
        );
    }

    #[test]
    fn every_shortcut_lands_in_a_rendered_group() {
        // The page renders GROUP_ORDER's cards and nothing else — a group()
        // arm returning a name missing from GROUP_ORDER would silently drop
        // its rows from Settings.
        for id in ShortcutId::ALL {
            assert!(
                GROUP_ORDER.contains(&group(id)),
                "{:?} is grouped under {:?}, which GROUP_ORDER does not render",
                id,
                group(id)
            );
        }
        // And every named group has at least one row — no empty cards.
        for name in GROUP_ORDER {
            assert!(
                ShortcutId::ALL.into_iter().any(|id| group(id) == name),
                "group {:?} would render an empty card",
                name
            );
        }
    }

    #[test]
    fn conflicting_records_are_refused() {
        // zeron parity: a combo bound elsewhere is refused at record time (the
        // helper names the owner) — conflicts never persist into the keymap.
        let keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("r", false, false, false, true) else {
            panic!("expected Set");
        };
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, &combo),
            Some(ShortcutId::ToggleChanges)
        );
        // Re-recording a shortcut's own combo is not a conflict.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleChanges, &combo),
            None
        );
        // A free combo conflicts with nothing.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-shift-x"),
            None
        );
    }

    #[test]
    fn modifier_send_labels_are_platform_specific() {
        assert_eq!(modifier_send_label(true), "⌘ Enter");
        assert_eq!(modifier_send_label(false), "Ctrl Enter");
    }

    #[test]
    fn modifier_send_reserves_its_combo_only_while_enabled() {
        assert!(!send_combo_is_reserved(
            ComposerSendBehavior::Enter,
            "mod-enter"
        ));
        assert!(send_combo_is_reserved(
            ComposerSendBehavior::ModEnter,
            "mod-enter"
        ));
        assert!(!send_combo_is_reserved(
            ComposerSendBehavior::ModEnter,
            "mod-shift-enter"
        ));
    }

    #[test]
    fn existing_modifier_enter_shortcut_blocks_activation() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(modifier_send_conflict_owner(&keymap), None);
        keymap.set(ShortcutId::NewSession, "mod-enter".into());
        assert_eq!(
            modifier_send_conflict_owner(&keymap),
            Some(ShortcutId::NewSession)
        );
    }
}
