//! Settings → Shortcuts (feature-inventory §1.4): a table of the rebindable
//! bindings — click a combo to record (Esc cancels), live conflict detection,
//! per-row Reset and Restore defaults. Changes emit [`ShortcutsEvent::Changed`];
//! the shell persists them and re-applies the app keymap.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::settings::{
    KeymapConfig, ShortcutId, combo_from_keystroke, conflicted_shortcuts, display_combo,
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
    Changed(KeymapConfig),
}

pub struct ShortcutsPage {
    /// Working copy (kept in sync with the shell via `Changed` events).
    keymap: KeymapConfig,
    recording: Option<ShortcutId>,
    focus: FocusHandle,
    // The page never talks RPC; state is kept for parity with sibling pages
    // (and future per-device keymaps).
    _state: Entity<AppState>,
}

impl EventEmitter<ShortcutsEvent> for ShortcutsPage {}

impl ShortcutsPage {
    pub fn new(state: Entity<AppState>, keymap: KeymapConfig, cx: &mut Context<Self>) -> Self {
        Self { keymap, recording: None, focus: cx.focus_handle(), _state: state }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::Changed(self.keymap.clone()));
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(recording) = self.recording else { return };
        let mods = &event.keystroke.modifiers;
        match record_key(&event.keystroke.key, mods.control, mods.alt, mods.shift, mods.platform)
        {
            RecordOutcome::Cancelled => {
                self.recording = None;
                cx.notify();
            }
            RecordOutcome::Ignored => {}
            RecordOutcome::Set(combo) => {
                self.keymap.set(recording, combo);
                self.recording = None;
                self.commit(cx);
            }
        }
        cx.stop_propagation();
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let conflicts = conflicted_shortcuts(&self.keymap);
        let recording = self.recording;

        let rows = ShortcutId::ALL.into_iter().enumerate().map(|(ix, id)| {
            let combo = self.keymap.get(id).to_string();
            let is_recording = recording == Some(id);
            let conflicted = conflicts.contains(&id);
            let non_default = combo != id.default_combo();
            let chip_text: SharedString = if is_recording {
                "Press keys… (Esc cancels)".into()
            } else {
                display_combo(&combo).into()
            };
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
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from(id.label())),
                        )
                        .when(conflicted, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.danger)
                                    .child(SharedString::from(
                                        "Conflicts with another shortcut",
                                    )),
                            )
                        }),
                )
                // Click-to-record combo chip.
                .child(
                    div()
                        .id(("shortcut-combo", ix))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(if conflicted {
                            theme.danger
                        } else if is_recording {
                            theme.accent
                        } else {
                            theme.border
                        })
                        .text_size(px(11.0))
                        .font_family(theme.font_mono.clone())
                        .text_color(if is_recording { theme.accent } else { theme.text })
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.recording = Some(id);
                            window.focus(&this.focus, cx);
                            cx.notify();
                        }))
                        .child(chip_text),
                )
                .child(
                    div()
                        .id(("shortcut-reset", ix))
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .text_size(px(11.0))
                        .text_color(if non_default { theme.text_muted } else { theme.text_faint })
                        .when(non_default, |el| {
                            el.cursor_pointer().hover(|s| s.bg(theme.element_hover))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keymap.reset(id);
                            this.recording = None;
                            this.commit(cx);
                        }))
                        .child(SharedString::from("Reset")),
                )
        });

        div()
            .id("shortcuts-page")
            .size_full()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_key_down(event, cx)
            }))
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
                            .child(SharedString::from("Keyboard shortcuts")),
                    )
                    .child(
                        div()
                            .id("shortcuts-restore-defaults")
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
                                this.keymap = KeymapConfig::default();
                                this.recording = None;
                                this.commit(cx);
                            }))
                            .child(SharedString::from("Restore defaults")),
                    ),
            )
            .children(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_outcomes() {
        assert_eq!(record_key("escape", false, false, false, false), RecordOutcome::Cancelled);
        assert_eq!(record_key("Escape", true, false, false, false), RecordOutcome::Cancelled);
        assert_eq!(
            record_key("s", true, false, false, false),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        // Bare modifiers stay recording.
        assert_eq!(record_key("shift", false, false, true, false), RecordOutcome::Ignored);
        assert_eq!(record_key("ctrl", true, false, false, false), RecordOutcome::Ignored);
    }

    #[test]
    fn record_then_conflict_then_reset_flow() {
        // Simulates the page's reducer path without a window: record mod-b onto
        // ToggleSidebar → conflict with ToggleChanges → reset clears it.
        let mut keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("b", true, false, false, false) else {
            panic!("expected Set");
        };
        keymap.set(ShortcutId::ToggleSidebar, combo);
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::ToggleSidebar));
        assert!(conflicts.contains(&ShortcutId::ToggleChanges));
        keymap.reset(ShortcutId::ToggleSidebar);
        assert!(conflicted_shortcuts(&keymap).is_empty());
        assert_eq!(keymap, KeymapConfig::default());
    }
}
