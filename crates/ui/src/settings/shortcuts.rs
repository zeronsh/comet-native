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
        Self {
            keymap,
            recording: None,
            focus: cx.focus_handle(),
            _state: state,
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::Changed(self.keymap.clone()));
        cx.notify();
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
                self.keymap.set(recording, combo);
                self.recording = None;
                self.commit(cx);
            }
        }
        cx.stop_propagation();
    }
}

/// One-line purpose copy per shortcut (comet settings.shortcuts.tsx rows).
fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::ToggleSidebar => "Show or hide the session sidebar",
        ShortcutId::ToggleChanges => "Show or hide the changes pane",
        ShortcutId::ToggleTerminal => "Show or hide the terminal panel",
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let conflicts = conflicted_shortcuts(&self.keymap);
        let recording = self.recording;
        let any_conflict = !conflicts.is_empty();

        let rows = ShortcutId::ALL.into_iter().enumerate().map(|(ix, id)| {
            let combo = self.keymap.get(id).to_string();
            let is_recording = recording == Some(id);
            let conflicted = conflicts.contains(&id);
            let non_default = combo != id.default_combo();
            let chip_text: SharedString = if is_recording {
                "Press keys…".into()
            } else {
                display_combo(&combo).into()
            };
            // comet settings.shortcuts.tsx row: min-h-[72px] px-5 gap-5, label
            // + description left, Reset (only when modified), then the combo
            // chip — recording inverts it to white-on-black.
            div()
                .min_h(px(72.0))
                .px(px(20.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(20.0))
                .when(ix > 0, |el| el.border_t_1().border_color(theme.border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(id.label())),
                        )
                        .child(
                            div()
                                .mt(px(2.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(description(id))),
                        ),
                )
                .when(non_default && !is_recording, |el| {
                    el.child(
                        div()
                            .id(("shortcut-reset", ix))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .cursor_pointer()
                            .hover(|s| s.text_color(Theme::dark().text))
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
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .map(|el| {
                            if is_recording {
                                el.border_color(theme.text.opacity(0.3))
                                    .bg(theme.text)
                                    .text_color(crate::theme::grey(0x0e))
                            } else if conflicted {
                                el.border_color(theme.danger.opacity(0.5))
                                    .bg(theme.bg)
                                    .text_color(theme.text)
                            } else {
                                el.border_color(theme.border)
                                    .bg(theme.bg)
                                    .text_color(theme.text)
                                    .hover(|s| {
                                        s.border_color(crate::theme::white_alpha(0.2))
                                            .bg(crate::theme::white_alpha(0.03))
                                    })
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.recording = Some(id);
                            window.focus(&this.focus, cx);
                            cx.notify();
                        }))
                        .child(chip_text),
                )
        });

        let helper: SharedString = if recording.is_some() {
            "Press Escape to cancel.".into()
        } else if any_conflict {
            "Shortcuts conflict — each combo must be unique.".into()
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
                                    .child(widgets::page_header(
                                        &theme,
                                        "Keyboard shortcuts",
                                        None,
                                    ))
                                    .child(widgets::page_subtitle(
                                        &theme,
                                        "Click a binding, then press the new combo.",
                                    )),
                            )
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("shortcuts-restore-defaults")
                                    .flex_none()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.keymap = KeymapConfig::default();
                                        this.recording = None;
                                        this.commit(cx);
                                    }))
                                    .child(SharedString::from("Restore defaults")),
                            ),
                    )
                    .child(widgets::section_card(&theme).mt(px(32.0)).children(rows))
                    .child(
                        div()
                            .mt(px(12.0))
                            .px(px(4.0))
                            .text_size(px(12.0))
                            .text_color(if any_conflict {
                                theme.danger.opacity(0.9)
                            } else {
                                theme.text_muted
                            })
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
            record_key("s", true, false, false, false),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
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
