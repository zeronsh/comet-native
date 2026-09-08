//! Settings → Appearance: system behavior, independent light/dark variants,
//! and the optional interactive accent overlay.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use zeron_theme::vscode::{ImportReport, SourceCompilation};
use zeron_theme::{
    AccentPreset, AccentSelection, CustomThemeEntry, CustomThemeStatus, InstallMode,
    SurfacePreference, SurfaceTreatment, ThemeRegistry, ThemeSelection,
};

use crate::appearance::{self, AppearanceMode};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::popover::{self, Popup};
use crate::settings::widgets;
use crate::theme::{Appearance, Theme};
use crate::theme_library;
use crate::typography::{self, FontAvailability, UiFontFamily, UiFontSize};

struct ImportDialog {
    input: Entity<ComposerInput>,
    _events: Subscription,
    focus: FocusHandle,
    focus_pending: bool,
    mode: InstallMode,
    compilation: Option<SourceCompilation>,
    selected: HashSet<String>,
    review_variant: Option<String>,
    error: Option<SharedString>,
}

pub struct AppearancePage {
    selected_font: UiFontFamily,
    selected_size: UiFontSize,
    font_focus: FocusHandle,
    size_focus: FocusHandle,
    font_menu: Popup<()>,
    size_menu: Popup<()>,
    font_menu_dismissed_at: Option<std::time::Instant>,
    size_menu_dismissed_at: Option<std::time::Instant>,
    light_theme_menu: Popup<()>,
    dark_theme_menu: Popup<()>,
    import_dialog: Option<ImportDialog>,
    review_entry: Option<String>,
    library_error: Option<SharedString>,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            selected_font: typography::effective(cx),
            selected_size: typography::font_size(cx),
            font_focus: cx.focus_handle(),
            size_focus: cx.focus_handle(),
            font_menu: Popup::default(),
            size_menu: Popup::default(),
            font_menu_dismissed_at: None,
            size_menu_dismissed_at: None,
            light_theme_menu: Popup::default(),
            dark_theme_menu: Popup::default(),
            import_dialog: None,
            review_entry: None,
            library_error: None,
        }
    }

    fn commit_font(&mut self, cx: &mut Context<Self>) {
        if typography::is_available(&self.selected_font, cx) {
            typography::set_family(self.selected_font.clone(), cx);
            self.selected_font = typography::effective(cx);
            self.close_font_menu(cx);
            cx.notify();
        }
    }

    fn commit_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        typography::set_font_size(self.selected_size, window, cx);
        self.selected_size = typography::font_size(cx);
        self.close_size_menu(cx);
        cx.notify();
    }

    fn close_font_menu(&mut self, cx: &mut Context<Self>) {
        if self.font_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.font_menu);
        }
    }

    fn close_size_menu(&mut self, cx: &mut Context<Self>) {
        if self.size_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.size_menu);
        }
    }

    fn dismiss_font_menu(&mut self, cx: &mut Context<Self>) {
        self.font_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_font_menu(cx);
    }

    fn dismiss_size_menu(&mut self, cx: &mut Context<Self>) {
        self.size_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_size_menu(cx);
    }

    fn toggle_font_menu(&mut self, cx: &mut Context<Self>) {
        self.close_size_menu(cx);
        let just_dismissed = self
            .font_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.font_menu.is_open() {
            self.close_font_menu(cx);
        } else if !just_dismissed {
            self.selected_font = typography::effective(cx);
            self.font_menu.open(());
        }
        cx.notify();
    }

    fn toggle_size_menu(&mut self, cx: &mut Context<Self>) {
        self.close_font_menu(cx);
        let just_dismissed = self
            .size_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.size_menu.is_open() {
            self.close_size_menu(cx);
        } else if !just_dismissed {
            self.selected_size = typography::font_size(cx);
            self.size_menu.open(());
        }
        cx.notify();
    }

    fn on_font_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let availability = typography::availability(cx);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, -1, &availability);
                cx.notify();
            }
            "down" | "right" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, 1, &availability);
                cx.notify();
            }
            "home" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = first_available(&availability);
                cx.notify();
            }
            "end" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = last_available(&availability);
                cx.notify();
            }
            "enter" | "space" => {
                if self.font_menu.is_open() {
                    self.commit_font(cx);
                } else {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
            }
            "escape" => {
                self.selected_font = typography::effective(cx);
                self.close_font_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn on_size_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = UiFontSize::ALL
            .iter()
            .position(|size| *size == self.selected_size)
            .unwrap_or(4);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[current.saturating_sub(1)];
                cx.notify();
            }
            "down" | "right" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[(current + 1).min(UiFontSize::ALL.len() - 1)];
                cx.notify();
            }
            "home" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[0];
                cx.notify();
            }
            "end" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[UiFontSize::ALL.len() - 1];
                cx.notify();
            }
            "enter" | "space" => {
                if self.size_menu.is_open() {
                    self.commit_size(window, cx);
                } else {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
            }
            "escape" => {
                self.selected_size = typography::font_size(cx);
                self.close_size_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn open_import(&mut self, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            ComposerInput::with_context(
                "Theme file, package.json, or extension folder",
                "PaletteSearch",
                cx,
            )
        });
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                let source = this
                    .import_dialog
                    .as_ref()
                    .map(|dialog| PathBuf::from(dialog.input.read(cx).text().trim()));
                if let Some(dialog) = this.import_dialog.as_mut()
                    && dialog
                        .compilation
                        .as_ref()
                        .zip(source.as_ref())
                        .is_some_and(|(compilation, source)| compilation.path != *source)
                {
                    dialog.compilation = None;
                    dialog.selected.clear();
                    dialog.review_variant = None;
                    dialog.error = None;
                    cx.notify();
                }
            }
            ComposerInputEvent::Submitted => {
                if this
                    .import_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.compilation.is_some())
                {
                    this.finish_import(cx);
                } else {
                    this.compile_import(cx);
                }
            }
            _ => {}
        });
        self.import_dialog = Some(ImportDialog {
            input,
            _events: events,
            focus: cx.focus_handle(),
            focus_pending: true,
            mode: InstallMode::Snapshot,
            compilation: None,
            selected: HashSet::new(),
            review_variant: None,
            error: None,
        });
        cx.notify();
    }

    fn compile_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        let source = dialog.input.read(cx).text().trim().to_owned();
        if source.is_empty() {
            dialog.error = Some("Choose a local theme file or extension folder.".into());
            cx.notify();
            return;
        }
        let path = PathBuf::from(&source);
        let family_name = source_name(&path);
        let family_id = format!("custom-{}", slug(&family_name));
        match theme_library::compile(&path, &family_id, &family_name) {
            Ok(compilation) => {
                dialog.selected = compilation
                    .family
                    .variants
                    .iter()
                    .map(|variant| variant.id.clone())
                    .collect();
                // Mapping diagnostics are useful, but they are an advanced
                // inspection surface rather than part of the happy path.
                dialog.review_variant = None;
                dialog.compilation = Some(compilation);
                dialog.error = None;
            }
            Err(error) => dialog.error = Some(error.to_string().into()),
        }
        cx.notify();
    }

    fn choose_import_source(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Choose Theme Source".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            let _ = this.update(cx, |page, cx| {
                if let Some(dialog) = page.import_dialog.as_mut() {
                    dialog.input.update(cx, |input, cx| {
                        input.set_text(path.display().to_string(), cx)
                    });
                }
                page.compile_import(cx);
            });
        })
        .detach();
    }

    fn finish_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        if dialog.selected.is_empty() {
            dialog.error = Some("Select at least one variant to import.".into());
            cx.notify();
            return;
        }
        let Some(compilation) = dialog.compilation.take() else {
            return;
        };
        let selected = dialog.selected.iter().cloned().collect::<Vec<_>>();
        match theme_library::install(compilation.clone(), &selected, dialog.mode, cx) {
            Ok(_) => self.import_dialog = None,
            Err(error) => {
                dialog.compilation = Some(compilation);
                dialog.error = Some(error.to_string().into());
            }
        }
        cx.notify();
    }
}

fn source_name(path: &Path) -> String {
    let path = if path.file_name().and_then(|name| name.to_str()) == Some("package.json") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Custom theme")
        .to_owned()
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if result.is_empty() {
        "theme".into()
    } else {
        result
    }
}

fn step_font(
    current: &UiFontFamily,
    delta: isize,
    availability: &FontAvailability,
) -> UiFontFamily {
    let choices = availability.choices();
    let current = choices
        .iter()
        .position(|family| family == current)
        .unwrap_or_default() as isize;
    let mut ix = current + delta.signum();
    while (0..choices.len() as isize).contains(&ix) {
        let candidate = &choices[ix as usize];
        if availability.is_available(candidate) {
            return candidate.clone();
        }
        ix += delta.signum();
    }
    choices[current as usize].clone()
}

fn first_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
}

fn last_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .rev()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
}

fn bar(fraction: f32, tone: Hsla) -> gpui::Div {
    div()
        .h(px(5.0))
        .w(gpui::relative(fraction))
        .rounded(px(3.0))
        .bg(tone)
}

fn accent_helper(accent: AccentSelection) -> String {
    match accent {
        AccentSelection::ThemeDefault => {
            "Theme default · Uses the palette's intended color.".into()
        }
        AccentSelection::Preset(preset) => format!(
            "{} · Controls, glyphs, selections, code, and activity.",
            preset.label()
        ),
    }
}

fn surface_label(surface: SurfacePreference) -> &'static str {
    match surface {
        SurfacePreference::ThemeDefault => "Theme default",
        SurfacePreference::Frosted => "Frosted",
        SurfacePreference::Opaque => "Opaque",
    }
}

fn surface_helper(surface: SurfacePreference, resolved: SurfaceTreatment) -> String {
    match surface {
        SurfacePreference::ThemeDefault => format!(
            "Uses this theme's {} default.",
            match resolved {
                SurfaceTreatment::Frosted => "frosted",
                SurfaceTreatment::Opaque => "opaque",
            }
        ),
        SurfacePreference::Frosted => "Theme-colored glass where supported.".into(),
        SurfacePreference::Opaque => "Solid surfaces for every theme.".into(),
    }
}

fn surface_choice(
    theme: &Theme,
    surface: SurfacePreference,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!(
            "appearance-surface-{}",
            surface_label(surface).to_lowercase().replace(' ', "-")
        )))
        .h(px(30.0))
        .px(px(10.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .bg(if selected {
            theme.accent_wash
        } else {
            theme.surface_raised.opacity(0.28)
        })
        .text_size(crate::typography::ui_rems(11.5))
        .font_weight(if selected {
            gpui::FontWeight::MEDIUM
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.accent
        } else {
            theme.text_muted
        })
        .flex()
        .items_center()
        .cursor_pointer()
        .when(!selected, |control| {
            control.hover(|style| style.bg(theme.surface_raised_hover))
        })
        .child(surface_label(surface))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Corners {
    All,
    Left,
    Right,
}

fn miniature(theme: &Theme, corners: Corners) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div().size_full().flex().flex_row().bg(theme.surface);
    let root = match corners {
        Corners::All => root.rounded(r),
        Corners::Left => root.rounded_tl(r).rounded_bl(r),
        Corners::Right => root.rounded_tr(r).rounded_br(r),
    };
    root.child(
        div()
            .w(px(44.0))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(8.0))
            .pt(px(14.0))
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .my(px(8.0))
            .mr(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(10.0))
            .child(bar(0.62, strong))
            .child(bar(0.88, line))
            .child(bar(0.76, line))
            .child(bar(0.52, line)),
    )
    .into_any_element()
}

fn miniature_split(
    themes: &ThemeSelection,
    accent: AccentSelection,
    surface: SurfacePreference,
) -> AnyElement {
    let light = Theme::for_selection(Appearance::Light, &themes.light, accent, surface);
    let dark = Theme::for_selection(Appearance::Dark, &themes.dark, accent, surface);
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&light, Corners::Left)),
        )
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&dark, Corners::Right)),
        )
        .into_any_element()
}

fn preview(
    mode: AppearanceMode,
    themes: &ThemeSelection,
    accent: AccentSelection,
    surface: SurfacePreference,
) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(themes, accent, surface),
        AppearanceMode::Light => miniature(
            &Theme::for_selection(Appearance::Light, &themes.light, accent, surface),
            Corners::All,
        ),
        AppearanceMode::Dark => miniature(
            &Theme::for_selection(Appearance::Dark, &themes.dark, accent, surface),
            Corners::All,
        ),
    }
}

fn model_appearance(appearance: Appearance) -> zeron_theme::Appearance {
    match appearance {
        Appearance::Dark => zeron_theme::Appearance::Dark,
        Appearance::Light => zeron_theme::Appearance::Light,
    }
}

fn palette_preview(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .rounded(px(5.0))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .flex()
        .child(div().w_1_3().h_full().bg(theme.surface))
        .child(div().w_1_3().h_full().bg(theme.bg))
        .child(div().w_1_3().h_full().bg(theme.accent))
}

fn compact_action(
    theme: &Theme,
    label: &str,
    id: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    popover::btn_ghost(theme, label, id.clone())
        .id(id)
        .h(px(28.0))
        .px(px(9.0))
        .py(px(0.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_raised.opacity(0.34))
        .flex()
        .items_center()
        .text_size(crate::typography::ui_rems(11.5))
}

fn import_scene_preview(variant: &zeron_theme::ThemeVariant) -> AnyElement {
    let theme = Theme::from_variant(
        variant,
        AccentSelection::ThemeDefault,
        SurfacePreference::ThemeDefault,
    );
    div()
        .w_full()
        .h(px(86.0))
        .flex()
        .gap(px(8.0))
        .child(
            div()
                .w(px(152.0))
                .h_full()
                .overflow_hidden()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .child(miniature(&theme, Corners::All)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .p(px(9.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_family(theme.font_mono.clone())
                        .child(
                            div()
                                .text_color(theme.syntax.keyword)
                                .child("fn ")
                                .child(div().text_color(theme.syntax.function).child("preview"))
                                .child(div().text_color(theme.syntax.punctuation).child("() {")),
                        ),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_family(theme.font_mono.clone())
                        .text_color(theme.syntax.string)
                        .child("  \"Theme mapping\""),
                )
                .child(
                    div()
                        .mt_auto()
                        .h(px(12.0))
                        .flex()
                        .rounded(px(3.0))
                        .overflow_hidden()
                        .children(
                            theme
                                .terminal
                                .ansi
                                .iter()
                                .take(8)
                                .map(|color| div().flex_1().h_full().bg(*color)),
                        ),
                ),
        )
        .child(
            div()
                .w(px(84.0))
                .h_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .p(px(8.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .h(px(12.0))
                        .rounded(px(3.0))
                        .bg(theme.diff_add.opacity(0.35)),
                )
                .child(
                    div()
                        .h(px(12.0))
                        .rounded(px(3.0))
                        .bg(theme.diff_del.opacity(0.35)),
                )
                .child(div().h(px(12.0)).rounded(px(3.0)).bg(theme.accent_wash)),
        )
        .into_any_element()
}

fn report_panel(theme: &Theme, report: &ImportReport) -> gpui::Stateful<gpui::Div> {
    let summary = format!(
        "{} mapped · {} adjusted · {} inferred/fallback · {} unsupported · {} warnings · {} validation",
        report.mappings.len(),
        report.adjustments.len(),
        report.fallbacks.len(),
        report.dropped.len(),
        report.warnings.len(),
        report.validation.len(),
    );
    div()
        .id(SharedString::from(format!(
            "theme-report-{}",
            report.source_hash
        )))
        .mt(px(8.0))
        .w_full()
        .max_h(px(168.0))
        .overflow_y_scroll()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_raised.opacity(0.35))
        .p(px(10.0))
        .text_size(crate::typography::ui_rems(11.0))
        .line_height(px(16.0))
        .text_color(theme.text_muted)
        .child(div().text_color(theme.text).child(summary))
        .children(report.adjustments.iter().map(|adjustment| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "Adjusted · {} {} → {} · {}",
                adjustment.zeron_role, adjustment.original, adjustment.resolved, adjustment.reason
            )))
        }))
        .children(report.fallbacks.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Fallback · {message}")))
        }))
        .children(report.warnings.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Warning · {message}")))
        }))
        .children(report.validation.iter().map(|issue| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "Validation {:?} {:?} · {}",
                issue.category, issue.severity, issue.message
            )))
        }))
        .children(report.dropped.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Unsupported · {message}")))
        }))
        .children(report.mappings.iter().map(|mapping| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "{} ← {}",
                mapping.zeron_role, mapping.vscode_key
            )))
        }))
}

fn accent_swatch(
    page_theme: &Theme,
    selection: AccentSelection,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let swatch_theme = Theme::for_selection(
        page_theme.appearance,
        page_theme.variant_id.as_ref(),
        selection,
        page_theme.surface_preference,
    );
    let sample = match selection {
        AccentSelection::ThemeDefault => div()
            .size_full()
            .rounded(px(6.0))
            .bg(swatch_theme.accent_wash)
            .flex()
            .items_center()
            .justify_center()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(4.0))
                    .h(px(13.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.light),
            )
            .child(
                div()
                    .w(px(4.0))
                    .h(px(16.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.mid),
            )
            .child(
                div()
                    .w(px(4.0))
                    .h(px(11.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.deep),
            ),
        AccentSelection::Preset(_) => div().size_full().rounded(px(6.0)).bg(swatch_theme.accent),
    };
    div()
        .id(SharedString::from(format!("accent-{}", selection.label())))
        .flex_none()
        .w(px(30.0))
        .h(px(34.0))
        .pb(px(4.0))
        .border_b_2()
        .border_color(if selected {
            swatch_theme.accent
        } else {
            gpui::transparent_black()
        })
        .cursor_pointer()
        .child(
            div()
                .size(px(30.0))
                .p(px(2.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if selected {
                    page_theme.border_strong
                } else {
                    page_theme.border
                })
                .bg(page_theme.surface_raised.opacity(0.42))
                .child(sample),
        )
}

impl AppearancePage {
    fn theme_menu(&self, appearance: Appearance) -> &Popup<()> {
        match appearance {
            Appearance::Light => &self.light_theme_menu,
            Appearance::Dark => &self.dark_theme_menu,
        }
    }

    fn theme_menu_mut(&mut self, appearance: Appearance) -> &mut Popup<()> {
        match appearance {
            Appearance::Light => &mut self.light_theme_menu,
            Appearance::Dark => &mut self.dark_theme_menu,
        }
    }

    fn close_theme_menu(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        if !self.theme_menu_mut(appearance).begin_close() {
            return;
        }
        match appearance {
            Appearance::Light => popover::reap_popup(cx, |page| &mut page.light_theme_menu),
            Appearance::Dark => popover::reap_popup(cx, |page| &mut page.dark_theme_menu),
        }
    }

    fn render_theme_selector(
        &mut self,
        appearance_kind: Appearance,
        selections: &ThemeSelection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let registry = ThemeRegistry::active();
        let selected_id = selections
            .variant_id(model_appearance(appearance_kind))
            .to_owned();
        let selected_variant = registry
            .variant(&selected_id)
            .or_else(|| {
                registry
                    .variants_for(model_appearance(appearance_kind))
                    .next()
            })
            .expect("the built-in registry has both appearances");
        let selected_theme = Theme::for_selection(
            appearance_kind,
            &selected_variant.id,
            AccentSelection::ThemeDefault,
            theme.surface_preference,
        );
        let open = self.theme_menu(appearance_kind).is_open();

        let mut trigger = div()
            .id(SharedString::from(format!(
                "{}-theme-selector",
                if appearance_kind.is_light() {
                    "light"
                } else {
                    "dark"
                }
            )))
            .relative()
            .flex_none()
            .w(px(218.0))
            .h(px(34.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_raised.opacity(if open { 0.75 } else { 0.42 }))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .when(!open, |el| {
                el.hover(|style| style.bg(theme.surface_raised_hover))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.theme_menu_mut(appearance_kind).note_trigger_press();
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.theme_menu_mut(appearance_kind).take_press_was_open() {
                    this.close_theme_menu(appearance_kind, cx);
                } else {
                    let other = if appearance_kind.is_light() {
                        Appearance::Dark
                    } else {
                        Appearance::Light
                    };
                    this.close_theme_menu(other, cx);
                    this.theme_menu_mut(appearance_kind).open(());
                }
                cx.notify();
            }))
            .child(palette_preview(&selected_theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(selected_variant.name.clone())),
            )
            .child(
                icons::icon(icons::SORT_VERTICAL)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.45 })),
            );

        if self.theme_menu(appearance_kind).get().is_some() {
            let closing = self.theme_menu(appearance_kind).closing_since();
            let heading = if appearance_kind.is_light() {
                "Light themes"
            } else {
                "Dark themes"
            };
            let menu = popover::popover_card(theme)
                .w(px(260.0))
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    this.close_theme_menu(appearance_kind, cx);
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, heading))
                .children(
                    registry
                        .variants_for(model_appearance(appearance_kind))
                        .enumerate()
                        .map(|(index, variant)| {
                            let id = variant.id.clone();
                            let name = variant.name.clone();
                            let active = id == selected_id;
                            let sample = Theme::for_selection(
                                appearance_kind,
                                &id,
                                AccentSelection::ThemeDefault,
                                theme.surface_preference,
                            );
                            popover::menu_row(
                                theme,
                                active,
                                SharedString::from(format!(
                                    "appearance-theme-menu-{appearance_kind:?}-{index}"
                                )),
                            )
                            .id(SharedString::from(format!(
                                "appearance-theme-row-{appearance_kind:?}-{index}"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                appearance::set_theme(appearance_kind, id.clone(), cx);
                                this.close_theme_menu(appearance_kind, cx);
                                cx.notify();
                            }))
                            .child(palette_preview(&sample))
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .when(active, |row| {
                                row.child(
                                    icons::icon(icons::CHECK)
                                        .size(px(14.0))
                                        .text_color(theme.accent),
                                )
                            })
                        }),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_below(
                SharedString::from(format!(
                    "appearance-{}-theme-menu",
                    if appearance_kind.is_light() {
                        "light"
                    } else {
                        "dark"
                    }
                )),
                menu,
                closing,
            ));
        }

        trigger.into_any_element()
    }

    fn render_import_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        {
            let dialog = self.import_dialog.as_mut()?;
            if std::mem::take(&mut dialog.focus_pending) {
                let input_focus = dialog.input.focus_handle(cx);
                window.focus(&input_focus, cx);
            }
        }
        let dialog = self.import_dialog.as_ref()?;
        let input = dialog.input.clone();
        let focus = dialog.focus.clone();
        let mode = dialog.mode;
        let compilation = dialog.compilation.clone();
        let selected = dialog.selected.clone();
        let review_variant = dialog.review_variant.clone();
        let error = dialog.error.clone();
        let ready = compilation.is_some() && !selected.is_empty();
        let hairline = crate::theme::hairline(0.08);

        let mode_control = |label: &'static str, description: &'static str, value: InstallMode| {
            let active = mode == value;
            div()
                .id(SharedString::from(format!(
                    "theme-import-mode-{}",
                    slug(label)
                )))
                .flex_1()
                .min_w_0()
                .p(px(10.0))
                .rounded(px(9.0))
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .bg(if active {
                    theme.accent_wash
                } else {
                    theme.surface_raised.opacity(0.28)
                })
                .cursor_pointer()
                .when(!active, |control| {
                    control.hover(|style| style.bg(theme.surface_raised_hover))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(dialog) = this.import_dialog.as_mut() {
                        dialog.mode = value;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(
                            div()
                                .size(px(16.0))
                                .rounded_full()
                                .border_1()
                                .border_color(if active {
                                    theme.accent
                                } else {
                                    theme.border_strong
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(active, |dot| {
                                    dot.child(div().size(px(8.0)).rounded_full().bg(theme.accent))
                                }),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if active { theme.text } else { theme.text_muted })
                                .child(label),
                        ),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .ml(px(23.0))
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.68))
                        .child(description),
                )
        };

        let section_label = |label: &'static str| {
            div()
                .mb(px(7.0))
                .text_size(crate::typography::ui_rems(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_muted)
                .child(label)
        };

        let mut main = div()
            .id("theme-import-main")
            .max_h(px(520.0))
            .overflow_y_scroll()
            .px(px(20.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .child(section_label("Source"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        popover::dialog_field(input.into_any_element())
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .py(px(0.0))
                            .flex()
                            .items_center(),
                    )
                    .child(
                        compact_action(theme, "Browse…", "theme-import-browse")
                            .h(px(36.0))
                            .px(px(12.0))
                            .flex_none()
                            .on_click(cx.listener(|this, _, _, cx| this.choose_import_source(cx))),
                    ),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .child(section_label("Keep it up to date"))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(mode_control(
                                "Import a copy",
                                "Works independently from the original file.",
                                InstallMode::Snapshot,
                            ))
                            .child(mode_control(
                                "Link to source",
                                "Reload changes from the file on disk.",
                                InstallMode::Link,
                            )),
                    ),
            );

        if let Some(ref compilation) = compilation {
            main = main.child(
                div()
                    .mt(px(18.0))
                    .pt(px(16.0))
                    .border_t_1()
                    .border_color(hairline)
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(section_label("Detected themes").mb(px(0.0)))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(format!(
                                "{} variant{}",
                                compilation.family.variants.len(),
                                if compilation.family.variants.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            ))),
                    ),
            );
            for variant in &compilation.family.variants {
                let variant_id = variant.id.clone();
                let selected_now = selected.contains(&variant_id);
                let review_open = review_variant.as_deref() == Some(variant_id.as_str());
                let appearance = if variant.appearance.is_dark() {
                    "Dark"
                } else {
                    "Light"
                };
                let report = compilation.reports.get(&variant.id);
                let sample = Theme::from_variant(
                    variant,
                    AccentSelection::ThemeDefault,
                    SurfacePreference::ThemeDefault,
                );
                main = main.child(
                    div()
                        .id(SharedString::from(format!("theme-import-row-{variant_id}")))
                        .mt(px(8.0))
                        .p(px(11.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(if selected_now {
                            theme.accent.opacity(0.7)
                        } else {
                            theme.border
                        })
                        .bg(if selected_now {
                            theme.accent_wash.opacity(0.42)
                        } else {
                            theme.surface_raised.opacity(0.22)
                        })
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(9.0))
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "theme-import-select-{variant_id}"
                                        )))
                                        .size(px(18.0))
                                        .rounded(px(5.0))
                                        .border_1()
                                        .border_color(if selected_now {
                                            theme.accent
                                        } else {
                                            theme.border_strong
                                        })
                                        .bg(if selected_now { theme.accent } else { theme.bg })
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .when(selected_now, |item| {
                                            item.child(
                                                icons::icon(icons::CHECK)
                                                    .size(px(12.0))
                                                    .text_color(theme.on_accent),
                                            )
                                        })
                                        .on_click(cx.listener({
                                            let variant_id = variant_id.clone();
                                            move |this, _, _, cx| {
                                                if let Some(dialog) = this.import_dialog.as_mut() {
                                                    if !dialog.selected.remove(&variant_id) {
                                                        dialog.selected.insert(variant_id.clone());
                                                    }
                                                }
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(palette_preview(&sample))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(12.5))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child(SharedString::from(variant.name.clone())),
                                        )
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.65))
                                                .child(appearance),
                                        ),
                                )
                                .child(
                                    compact_action(
                                        theme,
                                        if review_open {
                                            "Hide details"
                                        } else {
                                            "Details"
                                        },
                                        format!("theme-import-review-{variant_id}"),
                                    )
                                    .on_click(cx.listener({
                                        let variant_id = variant_id.clone();
                                        move |this, _, _, cx| {
                                            if let Some(dialog) = this.import_dialog.as_mut() {
                                                dialog.review_variant =
                                                    if dialog.review_variant.as_deref()
                                                        == Some(variant_id.as_str())
                                                    {
                                                        None
                                                    } else {
                                                        Some(variant_id.clone())
                                                    };
                                            }
                                            cx.notify();
                                        }
                                    })),
                                ),
                        )
                        .when(review_open, |row| {
                            row.child(
                                div()
                                    .mt(px(10.0))
                                    .pt(px(10.0))
                                    .border_t_1()
                                    .border_color(hairline)
                                    .child(import_scene_preview(variant)),
                            )
                            .when_some(report, |row, report| row.child(report_panel(theme, report)))
                        }),
                );
            }
            for failure in &compilation.failures {
                main = main.child(
                    div()
                        .mt(px(8.0))
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .bg(theme.warning.opacity(0.08))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(format!(
                            "{} could not be compiled · {}",
                            failure.name, failure.message
                        ))),
                );
            }
        } else {
            main = main.child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .items_start()
                    .gap(px(7.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.text_muted.opacity(0.72))
                    .child(
                        icons::icon(icons::INFO_CIRCLE)
                            .size(px(13.0))
                            .mt(px(1.0))
                            .flex_none(),
                    )
                    .child("Zeron finds light and dark variants automatically."),
            );
        }

        if let Some(error) = error {
            main = main.child(
                div()
                    .mt(px(12.0))
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.danger.opacity(0.08))
                    .flex()
                    .items_start()
                    .gap(px(7.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.danger)
                    .child(
                        icons::icon(icons::DANGER_TRIANGLE)
                            .size(px(13.0))
                            .flex_none()
                            .mt(px(1.0)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(error)),
            );
        }

        let header = div()
            .px(px(20.0))
            .pt(px(18.0))
            .pb(px(16.0))
            .flex()
            .items_start()
            .gap(px(16.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(popover::dialog_title(theme, "Add a theme"))
                    .child(
                        popover::dialog_body(
                            theme,
                            "Import a local theme into your library or keep it linked to its source.",
                        )
                        .mt(px(4.0)),
                    ),
            )
            .child(
                div()
                    .id("theme-import-close")
                    .size(px(28.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface_raised.opacity(0.28))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_raised_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_dialog = None;
                        cx.notify();
                    }))
                    .child(
                        icons::icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            );

        let footer = div()
            .border_t_1()
            .border_color(hairline)
            .bg(theme.surface_raised.opacity(0.18))
            .px(px(20.0))
            .py(px(12.0))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .child(
                compact_action(theme, "Cancel", "theme-import-cancel")
                    .h(px(34.0))
                    .px(px(13.0))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_dialog = None;
                        cx.notify();
                    })),
            )
            .child(
                popover::btn_primary(
                    theme,
                    if compilation.is_some() {
                        "Import selected"
                    } else {
                        "Analyze theme"
                    },
                )
                .id("theme-import-action")
                .h(px(34.0))
                .px(px(14.0))
                .py(px(0.0))
                .flex()
                .items_center()
                .when(compilation.is_some() && !ready, |button| {
                    button.opacity(0.45)
                })
                .when(compilation.is_none() || ready, |button| {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        if this
                            .import_dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.compilation.is_some())
                        {
                            this.finish_import(cx);
                        } else {
                            this.compile_import(cx);
                        }
                    }))
                }),
            );

        let card = popover::dialog_card(theme)
            .id("theme-import-card")
            .w(px(600.0))
            .max_h(px(760.0))
            .p(px(0.0))
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                match popover::classify_key(
                    event.keystroke.key.as_str(),
                    event.keystroke.modifiers.platform,
                    event.keystroke.modifiers.control,
                ) {
                    popover::MenuKey::Escape => {
                        this.import_dialog = None;
                        cx.notify();
                    }
                    popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                        if this
                            .import_dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.compilation.is_some())
                        {
                            this.finish_import(cx);
                        } else {
                            this.compile_import(cx);
                        }
                    }
                    _ => {}
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.import_dialog = None;
                cx.notify();
            }))
            .child(header)
            .child(main)
            .child(footer)
            .into_any_element();

        Some(popover::modal("theme-import-dialog", viewport, card))
    }

    fn render_review_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entry_id = self.review_entry.as_ref()?;
        let entry = theme_library::entries(cx)
            .into_iter()
            .find(|entry| &entry.id == entry_id)?;
        let mut card = popover::dialog_card(theme)
            .id("theme-review-card")
            .w(px(660.0))
            .max_h(px(720.0))
            .overflow_y_scroll()
            .child(popover::dialog_title(theme, "Theme mapping"))
            .child(
                popover::dialog_body(theme, format!("{} · {}", entry.name, entry.source.label()))
                    .mt(px(6.0)),
            );
        for variant in &entry.family.variants {
            card = card
                .child(
                    div()
                        .mt(px(14.0))
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(SharedString::from(variant.name.clone())),
                )
                .child(import_scene_preview(variant));
            if let Some(report) = entry.reports.get(&variant.id) {
                card = card.child(report_panel(theme, report));
            }
        }
        card = card.child(
            div().mt(px(16.0)).flex().justify_end().child(
                popover::btn_primary(theme, "Done")
                    .id("theme-review-close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.review_entry = None;
                        cx.notify();
                    })),
            ),
        );
        Some(popover::modal(
            "theme-review-dialog",
            viewport,
            card.into_any_element(),
        ))
    }

    fn render_library_entry(
        &mut self,
        entry: CustomThemeEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = entry.id.clone();
        let linked = entry.source.is_linked();
        let source = entry
            .source
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Self-contained snapshot".into());
        let status = match &entry.status {
            CustomThemeStatus::Ready => format!(
                "{} · {} variant{} · {}",
                entry.source.label(),
                entry.family.variants.len(),
                if entry.family.variants.len() == 1 {
                    ""
                } else {
                    "s"
                },
                source
            ),
            CustomThemeStatus::Warning { message } => {
                format!("Using last known good · {message}")
            }
        };
        widgets::card_row(theme, false)
            .child(widgets::row_tile(
                theme,
                if linked {
                    icons::GLOBAL
                } else {
                    icons::DOCUMENT
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(widgets::row_title(theme, &entry.name))
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(
                                if matches!(entry.status, CustomThemeStatus::Warning { .. }) {
                                    theme.warning
                                } else {
                                    theme.text_muted
                                },
                            )
                            .child(SharedString::from(status)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .when(linked, |actions| {
                        actions.child(
                            compact_action(theme, "Reload", format!("theme-reload-{id}")).on_click(
                                cx.listener({
                                    let id = id.clone();
                                    move |_, _, _, cx| {
                                        let _ = theme_library::reload(&id, cx);
                                        cx.notify();
                                    }
                                }),
                            ),
                        )
                    })
                    .child(
                        compact_action(theme, "Reveal", format!("theme-reveal-{id}")).on_click(
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    if let Err(error) = theme_library::reveal(&id, cx) {
                                        this.library_error = Some(error.to_string().into());
                                    }
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child(
                        compact_action(theme, "Review", format!("theme-review-{id}")).on_click(
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.review_entry = Some(id.clone());
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child(
                        compact_action(
                            theme,
                            "Duplicate as editable",
                            format!("theme-duplicate-{id}"),
                        )
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| {
                                if let Err(error) = theme_library::duplicate_as_editable(&id, cx) {
                                    this.library_error = Some(error.to_string().into());
                                }
                                cx.notify();
                            }
                        })),
                    )
                    .when(linked, |actions| {
                        actions.child(
                            compact_action(theme, "Unlink", format!("theme-unlink-{id}")).on_click(
                                cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        if let Err(error) = theme_library::unlink(&id, cx) {
                                            this.library_error = Some(error.to_string().into());
                                        }
                                        cx.notify();
                                    }
                                }),
                            ),
                        )
                    })
                    .child(
                        compact_action(theme, "Remove", format!("theme-remove-{id}"))
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(error) = theme_library::remove(&id, cx) {
                                    this.library_error = Some(error.to_string().into());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_theme_library_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entries = theme_library::entries(cx);
        let (linked, imported): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| entry.source.is_linked());
        let mut rows = vec![
            widgets::card_row(theme, false)
                .child(widgets::row_tile(theme, icons::FOLDER_WITH_FILES))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(widgets::row_title(theme, "Theme library"))
                        .child(widgets::meta_line(
                            theme,
                            vec![
                                div()
                                    .child("Import or link custom themes.")
                                    .into_any_element(),
                            ],
                        )),
                )
                .child(
                    popover::btn_primary(theme, "Add theme")
                        .id("theme-library-add")
                        .on_click(cx.listener(|this, _, _, cx| this.open_import(cx))),
                )
                .into_any_element(),
        ];
        if !imported.is_empty() {
            rows.push(
                div()
                    .px(px(16.0))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child("IMPORTED")
                    .into_any_element(),
            );
            rows.extend(
                imported
                    .into_iter()
                    .map(|entry| self.render_library_entry(entry, theme, cx)),
            );
        }
        if !linked.is_empty() {
            rows.push(
                div()
                    .px(px(16.0))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child("LINKED")
                    .into_any_element(),
            );
            rows.extend(
                linked
                    .into_iter()
                    .map(|entry| self.render_library_entry(entry, theme, cx)),
            );
        }
        rows
    }
}

impl Render for AppearancePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let effective_font = typography::effective(cx);
        let requested_font = typography::requested(cx);
        let availability = typography::availability(cx);
        let fixed = theme.font_sans_fixed.clone();
        let current_mode = appearance::mode(cx);
        let current_themes = appearance::themes(cx);
        let current_accent = appearance::accent(cx);
        let current_surface = appearance::surface(cx);
        let cards = AppearanceMode::ALL
            .into_iter()
            .map(|mode| {
                widgets::option_card(
                    &theme,
                    mode.label(),
                    mode == current_mode,
                    preview(mode, &current_themes, current_accent, current_surface),
                )
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
            })
            .collect::<Vec<_>>();

        let mut theme_rows = Vec::new();
        for (index, appearance_kind) in [Appearance::Light, Appearance::Dark]
            .into_iter()
            .enumerate()
        {
            let label = if appearance_kind.is_light() {
                "Light theme"
            } else {
                "Dark theme"
            };
            let selector = self.render_theme_selector(appearance_kind, &current_themes, &theme, cx);
            theme_rows.push(
                widgets::card_row(&theme, index == 0)
                    .child(widgets::row_tile(&theme, icons::TUNING))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(&theme, label))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Used whenever this appearance is active.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(selector)
                    .into_any_element(),
            );
        }

        let mut accent_choices = vec![AccentSelection::ThemeDefault];
        accent_choices.extend(AccentPreset::ALL.map(AccentSelection::Preset));
        let accent_controls = accent_choices
            .into_iter()
            .map(|selection| {
                let selected = selection == current_accent;
                accent_swatch(&theme, selection, selected).on_click(cx.listener(
                    move |_, _, _, cx| {
                        appearance::set_accent(selection, cx);
                        cx.notify();
                    },
                ))
            })
            .collect::<Vec<_>>();
        let surface_controls = SurfacePreference::ALL
            .into_iter()
            .map(|surface| {
                surface_choice(&theme, surface, surface == current_surface).on_click(cx.listener(
                    move |_, _, _, cx| {
                        appearance::set_surface(surface, cx);
                        cx.notify();
                    },
                ))
            })
            .collect::<Vec<_>>();
        let mut settings_rows = theme_rows;
        settings_rows.push(
            widgets::card_row(&theme, false)
                .child(widgets::row_tile(&theme, icons::TUNING))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(widgets::row_title(&theme, "Accent color"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child(SharedString::from(accent_helper(current_accent)))
                                    .into_any_element(),
                            ],
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .ml(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(accent_controls),
                )
                .into_any_element(),
        );
        settings_rows.push(
            widgets::card_row(&theme, false)
                .child(widgets::row_tile(&theme, icons::WIDGET))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(widgets::row_title(&theme, "Glass"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child(SharedString::from(surface_helper(
                                        current_surface,
                                        theme.surface_treatment,
                                    )))
                                    .into_any_element(),
                            ],
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .ml(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(surface_controls),
                )
                .into_any_element(),
        );
        settings_rows.extend(self.render_theme_library_rows(&theme, cx));
        let library_warning = self
            .library_error
            .clone()
            .or_else(|| theme_library::load_warning(cx).map(SharedString::from));
        let modal = self
            .render_import_dialog(window.viewport_size(), &theme, window, cx)
            .or_else(|| self.render_review_dialog(window.viewport_size(), &theme, cx));

        let font_rows: Vec<AnyElement> = availability
            .choices()
            .iter()
            .cloned()
            .enumerate()
            .map(|(ix, family)| {
                let available = availability.is_available(&family);
                let selected = family == effective_font;
                let focused = family == self.selected_font;
                let label = SharedString::from(family.label().to_owned());
                popover::menu_row_nav(
                    &theme,
                    selected,
                    focused,
                    format!("interface-font-option-{ix}"),
                )
                .id(("interface-font-option", ix))
                .when(available, |row| {
                    row.on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.selected_font = family.clone();
                        this.commit_font(cx);
                    }))
                })
                .when(!available, |row| row.opacity(0.45))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(div().w(px(18.0)).flex_none().when(selected, |slot| {
                    slot.child(
                        icons::icon(icons::CHECK)
                            .size(px(14.0))
                            .text_color(theme.accent),
                    )
                }))
                .into_any_element()
            })
            .collect();

        let font_menu = popover::popover_card(&theme)
            .id("interface-font-scroll")
            .w(px(220.0))
            .font_family(fixed.clone())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_font_menu(cx)))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(font_rows)
            .into_any_element();

        let font_trigger = div()
            .id("interface-font-dropdown")
            .relative()
            .w(px(220.0))
            .h(px(36.0))
            .px(px(11.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if self.font_menu.is_open() {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(crate::theme::ink(0.025))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .track_focus(&self.font_focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_font_key_down(event, cx)),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.font_focus, cx);
                this.toggle_font_menu(cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(effective_font.label().to_owned())),
            )
            .child(
                icons::icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.font_menu.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "interface-font-menu",
                    font_menu,
                    self.font_menu.closing_since(),
                ))
            });

        let size_rows: Vec<AnyElement> = UiFontSize::ALL
            .into_iter()
            .enumerate()
            .map(|(ix, size)| {
                popover::menu_row_nav(
                    &theme,
                    size == typography::font_size(cx),
                    size == self.selected_size,
                    format!("interface-font-size-option-{ix}"),
                )
                .id(("interface-font-size-option", ix))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.selected_size = size;
                    this.commit_size(window, cx);
                }))
                .child(div().flex_1().child(size.label()))
                .child(div().w(px(18.0)).flex_none().when(
                    size == typography::font_size(cx),
                    |slot| {
                        slot.child(
                            icons::icon(icons::CHECK)
                                .size(px(14.0))
                                .text_color(theme.accent),
                        )
                    },
                ))
                .into_any_element()
            })
            .collect();

        let size_menu = popover::popover_card(&theme)
            .w(px(128.0))
            .font_family(fixed.clone())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_size_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(size_rows)
            .into_any_element();

        let size_trigger = div()
            .id("interface-font-size-dropdown")
            .relative()
            .w(px(128.0))
            .h(px(36.0))
            .px(px(11.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if self.size_menu.is_open() {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(crate::theme::ink(0.025))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .track_focus(&self.size_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_size_key_down(event, window, cx)
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.size_focus, cx);
                this.toggle_size_menu(cx);
            }))
            .child(div().flex_1().child(typography::font_size(cx).label()))
            .child(
                icons::icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.size_menu.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "interface-font-size-menu",
                    size_menu,
                    self.size_menu.closing_since(),
                ))
            });

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Appearance", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose how Zeron looks. These settings stay on this device.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Appearance"))
                            .child(widgets::option_card_row().children(cards)),
                    )
                    .child(widgets::section_card(&theme).children(settings_rows))
                    .child(
                        div()
                            .mt(px(36.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .font_family(fixed.clone())
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(24.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.0))
                                            .child(widgets::field_label(&theme, "Interface font"))
                                            .child(
                                                div()
                                                    .max_w(px(520.0))
                                                    .text_size(typography::ui_rems(12.0))
                                                    .line_height(px(18.0))
                                                    .text_color(theme.text_muted)
                                                    .child(SharedString::from(
                                                        "Used across the interface and conversations. Code, diffs, and terminal keep their current fonts and sizes.",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(font_trigger)
                                            .child(size_trigger),
                                    ),
                            )
                            .when(requested_font != effective_font, |section| {
                                section.child(
                                    widgets::error_strip(
                                        &theme,
                                        "This font could not be loaded. Comet is using Geist.",
                                    )
                                    .font_family(fixed.clone()),
                                )
                            }),
                    )
                    .when_some(library_warning, |page, warning| {
                        page.child(
                            div()
                                .mt(px(8.0))
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.warning)
                                .child(warning),
                        )
                    }),
            )
            .children(modal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_card() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn registry_offers_both_appearances_and_keeps_single_dark_families_valid() {
        let registry = ThemeRegistry::builtin();
        assert_eq!(
            registry
                .variants_for(zeron_theme::Appearance::Light)
                .count(),
            10
        );
        assert_eq!(
            registry.variants_for(zeron_theme::Appearance::Dark).count(),
            20
        );
    }

    #[test]
    fn accent_helper_explains_default_and_override_scope() {
        assert!(accent_helper(AccentSelection::ThemeDefault).contains("intended"));
        let copy = accent_helper(AccentSelection::Preset(AccentPreset::Pink));
        assert!(copy.starts_with("Pink ·"));
        assert!(copy.contains("glyphs"));
    }

    #[test]
    fn surface_helper_explains_theme_default_and_global_overrides() {
        let default = surface_helper(SurfacePreference::ThemeDefault, SurfaceTreatment::Opaque);
        assert!(default.contains("opaque default"));
        assert!(
            surface_helper(SurfacePreference::Frosted, SurfaceTreatment::Opaque)
                .contains("where supported")
        );
        assert!(
            surface_helper(SurfacePreference::Opaque, SurfaceTreatment::Frosted)
                .contains("every theme")
        );
    }

    #[test]
    fn font_options_appear_once_in_stable_order() {
        let catalog = FontAvailability::all();
        let labels: Vec<_> = catalog.choices().iter().map(UiFontFamily::label).collect();
        assert_eq!(labels.len(), 5);
        assert_eq!(
            labels,
            ["Geist", "Geist Mono", "System UI", "Arial", "Menlo"]
        );
        let unique = labels.into_iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn font_keyboard_navigation_stops_at_edges_and_skips_unavailable() {
        let all = FontAvailability::all();
        assert_eq!(
            step_font(&UiFontFamily::Geist, -1, &all),
            UiFontFamily::Geist
        );
        assert_eq!(
            step_font(&UiFontFamily::Installed("Menlo".into()), 1, &all),
            UiFontFamily::Installed("Menlo".into())
        );
        let without_arial = all.without(&UiFontFamily::Installed("Arial".into()));
        assert_eq!(
            step_font(&UiFontFamily::System, 1, &without_arial),
            UiFontFamily::Installed("Menlo".into())
        );
    }

    #[test]
    fn font_size_options_are_ordered_and_include_the_default() {
        let values = UiFontSize::ALL.map(UiFontSize::pixels);
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(UiFontSize::ALL.contains(&UiFontSize::default()));
    }
}
