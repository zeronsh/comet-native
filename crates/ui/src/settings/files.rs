//! Settings → Files: local preferences for workspace-file editing.

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use super::widgets;
use crate::{icons, theme::Theme};

const DELAY_OPTIONS: [u64; 5] = [300, 600, 900, 1_500, 3_000];
const FONT_SIZE_OPTIONS: [f32; 5] = [10.0, 11.5, 13.0, 15.0, 17.0];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilesSettingsEvent {
    AutosaveChanged(bool),
    AutosaveDelayChanged(u64),
    EditorFontSizeChanged(f32),
    WordWrapChanged(bool),
    ShowAllFilesChanged(bool),
}

pub struct FilesSettingsPage {
    autosave_enabled: bool,
    autosave_delay_ms: u64,
    editor_font_size: f32,
    word_wrap: bool,
    show_all_files: bool,
}

impl EventEmitter<FilesSettingsEvent> for FilesSettingsPage {}

impl FilesSettingsPage {
    pub fn new(
        autosave_enabled: bool,
        autosave_delay_ms: u64,
        editor_font_size: f32,
        word_wrap: bool,
        show_all_files: bool,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            autosave_enabled,
            autosave_delay_ms,
            editor_font_size,
            word_wrap,
            show_all_files,
        }
    }

    pub fn set_word_wrap(&mut self, word_wrap: bool, cx: &mut Context<Self>) {
        if self.word_wrap == word_wrap {
            return;
        }
        self.word_wrap = word_wrap;
        cx.notify();
    }

    pub fn set_show_all_files(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        if self.show_all_files == show_all_files {
            return;
        }
        self.show_all_files = show_all_files;
        cx.notify();
    }
}

impl Render for FilesSettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let autosave_enabled = self.autosave_enabled;
        let selected = self.autosave_delay_ms;
        let selected_font_size = self.editor_font_size;
        let word_wrap = self.word_wrap;
        let show_all_files = self.show_all_files;
        let options = DELAY_OPTIONS.into_iter().map(|delay| {
            let active = delay == selected;
            div()
                .id(SharedString::from(format!("files-autosave-{delay}")))
                .h(px(28.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(if active {
                    theme.accent.opacity(0.7)
                } else {
                    theme.border
                })
                .bg(if active {
                    theme.accent.opacity(0.11)
                } else {
                    crate::theme::wash(0.025)
                })
                .text_size(px(11.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::wash(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.autosave_delay_ms = delay;
                    cx.emit(FilesSettingsEvent::AutosaveDelayChanged(delay));
                    cx.notify();
                }))
                .child(if delay >= 1_000 {
                    format!("{} s", delay as f32 / 1_000.0)
                } else {
                    format!("{delay} ms")
                })
        });
        let font_size_options = FONT_SIZE_OPTIONS.into_iter().map(|font_size| {
            let active = font_size == selected_font_size;
            div()
                .id(SharedString::from(format!(
                    "files-editor-font-size-{font_size}"
                )))
                .h(px(28.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(if active {
                    theme.accent.opacity(0.7)
                } else {
                    theme.border
                })
                .bg(if active {
                    theme.accent.opacity(0.11)
                } else {
                    crate::theme::wash(0.025)
                })
                .text_size(px(11.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::wash(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.editor_font_size = font_size;
                    cx.emit(FilesSettingsEvent::EditorFontSizeChanged(font_size));
                    cx.notify();
                }))
                .child(if font_size.fract() == 0.0 {
                    format!("{font_size:.0} px")
                } else {
                    format!("{font_size:.1} px")
                })
        });

        let card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::FOLDER))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Autosave"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("Save edited workspace files to disk automatically.")
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, autosave_enabled)
                            .id("files-autosave-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.autosave_enabled = !this.autosave_enabled;
                                cx.emit(FilesSettingsEvent::AutosaveChanged(this.autosave_enabled));
                                cx.notify();
                            })),
                    ),
            )
            .when(autosave_enabled, |card| {
                card.child(
                    widgets::card_row(&theme, true)
                        .items_start()
                        .child(widgets::row_tile(&theme, icons::FOLDER))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(&theme, "Autosave delay"))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![div()
                                        .child(
                                            "Save files after editing has been idle for this long.",
                                        )
                                        .into_any_element()],
                                ))
                                .child(
                                    div()
                                        .mt(px(12.0))
                                        .flex()
                                        .flex_wrap()
                                        .gap(px(7.0))
                                        .children(options),
                                ),
                        ),
                )
            })
            .child(
                widgets::card_row(&theme, true)
                    .items_start()
                    .child(widgets::row_tile(&theme, icons::TUNING))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Editor font size"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("Set the text size in workspace file editors.")
                                        .into_any_element(),
                                ],
                            ))
                            .child(
                                div()
                                    .mt(px(12.0))
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(7.0))
                                    .children(font_size_options),
                            ),
                    ),
            )
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::LIST))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Word wrap"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child("Wrap long lines in every workspace file.")
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, word_wrap)
                            .id("files-word-wrap-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.word_wrap = !this.word_wrap;
                                cx.emit(FilesSettingsEvent::WordWrapChanged(this.word_wrap));
                                cx.notify();
                            })),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::EYE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Show all files"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(
                                            "Include hidden and ignored files in every file tree.",
                                        )
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, show_all_files)
                            .id("files-show-all-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.show_all_files = !this.show_all_files;
                                cx.emit(FilesSettingsEvent::ShowAllFilesChanged(
                                    this.show_all_files,
                                ));
                                cx.notify();
                            })),
                    ),
            );

        div()
            .id("files-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Files", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Control how workspace files are displayed and saved while you edit.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(card),
            )
    }
}
