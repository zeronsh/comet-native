//! Workspace file browsing surface.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, ListAlignment, ListState, Pixels, Point, Render,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::ListWorkspaceDirectoryRequest;

use crate::{
    composer::{ComposerInput, ComposerInputEvent},
    state::AppState,
};

pub mod client;
pub mod document;
pub mod editor;
pub mod editor_adapter;
mod markdown_media;
mod markdown_preview;
pub mod model;
pub mod preview;
pub mod search;
pub mod tree;
pub mod watch;

use client::{FilesRequestContext, WorkspaceFilesClient};
use model::{DirectoryLoadState, FileTreeModel};
use preview::FilePreviewState;
use search::FileSearchState;

static NEXT_REVIEW_COMMENT_FLUSH_SOURCE: AtomicU64 = AtomicU64::new(1);
use crate::surface_chrome::{
    CONTROL_RADIUS as TOOLBAR_BUTTON_RADIUS, CONTROL_SIZE as TOOLBAR_BUTTON_SIZE, toolbar,
};

pub(super) fn toolbar_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(TOOLBAR_BUTTON_SIZE))
        .flex_none()
        .rounded(px(TOOLBAR_BUTTON_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(label)
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
            window.prevent_default()
        })
        .hover(|style| style.bg(crate::theme::wash(0.14)))
        .tooltip(move |_, cx| {
            cx.new(|_| preview::FileEditorTooltip { text: label.into() })
                .into()
        })
        .tooltip_show_delay(Duration::from_millis(350))
}

/// A workspace-relative file or directory dragged out of a Files surface.
///
/// Keeping the payload relative is important: the composer may target a
/// remote device, and its existing file-mention transport resolves paths in
/// that workspace instead of leaking a path from the UI machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspacePathDrag {
    pub path: String,
    pub is_directory: bool,
}

impl WorkspacePathDrag {
    pub(crate) fn new(path: String, is_directory: bool) -> Self {
        Self { path, is_directory }
    }

    fn title(&self) -> SharedString {
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
            .to_string()
            .into()
    }
}

/// Compact drag preview shared by tree and search rows. It deliberately uses
/// the same raised surface, hairline, type scale, and opacity as surface tabs.
pub(crate) struct WorkspacePathDragGhost {
    payload: WorkspacePathDrag,
}

impl Render for WorkspacePathDragGhost {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::Theme::of(cx);
        div()
            .h(px(24.0))
            .max_w(px(220.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(11.5))
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                crate::icons::icon(if self.payload.is_directory {
                    crate::icons::FOLDER
                } else {
                    crate::icons::DOCUMENT
                })
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(div().min_w_0().truncate().child(self.payload.title()))
    }
}

pub(crate) fn workspace_path_drag_ghost(
    payload: &WorkspacePathDrag,
    cx: &mut gpui::App,
) -> gpui::Entity<WorkspacePathDragGhost> {
    let payload = payload.clone();
    cx.new(|_| WorkspacePathDragGhost { payload })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesEvent {
    OpenFile(String),
    TitleChanged,
    FileRenamed { old_path: String, new_path: String },
    WordWrapChanged(bool),
    ShowAllFilesChanged(bool),
    CloseReady,
    CloseCancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesCloseDisposition {
    Allow,
    Pending,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilesPresentation {
    Browser,
    Editor,
}

impl FilesPresentation {
    fn is_editor(self) -> bool {
        matches!(self, Self::Editor)
    }
}

impl EventEmitter<FilesEvent> for FilesSurface {}

struct EditorContextMenu {
    editor: Entity<editor::FileEditorState>,
    position: Point<Pixels>,
    availability: editor::EditorMenuAvailability,
}

pub struct FilesSurface {
    state: Entity<AppState>,
    chat_id: String,
    review_comment_flush_source: u64,
    presentation: FilesPresentation,
    editor_path: Option<String>,
    request_context: Option<FilesRequestContext>,
    target_change_pending: bool,
    pending_request_context: Option<FilesRequestContext>,
    tree: FileTreeModel,
    tree_list: ListState,
    tree_focus: FocusHandle,
    search: Entity<ComposerInput>,
    search_state: FileSearchState,
    search_list: ListState,
    watch_task: Option<Task<()>>,
    watch_sequence: Option<u64>,
    watch_error: Option<SharedString>,
    preview: FilePreviewState,
    editor_context_menu: crate::popover::Popup<EditorContextMenu>,
    loads: HashMap<(String, Option<String>), Task<()>>,
    error: Option<SharedString>,
    started: bool,
    _observe: Subscription,
    _search_events: Subscription,
}

impl Render for FilesSurface {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::Theme::of(cx).clone();
        let phase = self.tree.node("").map(|root| root.load.clone());
        let content = if !self.search_state.query.is_empty() {
            self.render_search_results(cx)
        } else if let Some(error) = self.error.clone() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .px(px(28.0))
                .child(
                    div()
                        .text_center()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(error),
                )
                .child(
                    div()
                        .id("files-retry-root")
                        .h(px(28.0))
                        .px(px(12.0))
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(crate::theme::wash(0.04))
                        .hover(|style| style.bg(crate::theme::wash(0.09)))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(theme.text)
                        .child("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_root(cx))),
                )
                .into_any_element()
        } else if matches!(
            phase.as_ref(),
            Some(DirectoryLoadState::Unloaded | DirectoryLoadState::Loading { .. })
        ) {
            div().flex_1().into_any_element()
        } else {
            self.render_tree(cx)
        };
        let split_editor = self.presentation.is_editor() && self.preview.has_active();
        let watch_error = self.watch_error.clone();
        let tree_pane = div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(!split_editor, |pane| {
                pane.child(self.render_header(&theme, cx))
            })
            .when_some(watch_error, |element, error| {
                element.child(
                    div()
                        .h(px(27.0))
                        .flex_none()
                        .px(px(10.0))
                        .border_b_1()
                        .border_color(theme.warning.opacity(0.22))
                        .bg(theme.warning.opacity(0.045))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(10.0))
                        .text_color(theme.warning_muted)
                        .child(
                            crate::icons::icon(crate::icons::REFRESH)
                                .size(px(10.5))
                                .flex_none(),
                        )
                        .child(div().min_w_0().flex_1().truncate().child(error))
                        .child(
                            div()
                                .id("files-watch-refresh-now")
                                .h(px(20.0))
                                .flex_none()
                                .px(px(6.0))
                                .rounded(px(5.0))
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .role(gpui::Role::Button)
                                .aria_label("Refresh workspace files now")
                                .text_color(theme.text_muted)
                                .hover(|style| style.bg(crate::theme::wash(0.07)))
                                .child("Refresh now")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.refresh(cx);
                                    this.reconcile_open_documents(cx);
                                })),
                        ),
                )
            })
            .child(content);
        let is_editor = self.presentation.is_editor();
        let mut header = None;
        let body = if split_editor {
            let wide = self.preview.is_wide();
            let tree_width = if wide {
                self.preview.tree_width()
            } else {
                self.preview.narrow_tree_width()
            };
            let openness = self.preview.tree_sidebar_frame(window, cx);
            // Same arrangement as the outer right-sidebar toggle: the trigger
            // is outside the animated controls, in a permanently mounted slot.
            let toggle_width =
                crate::surface_chrome::CONTROL_SIZE + crate::surface_chrome::EDGE_INSET;
            let tree_header = self
                .render_header(&theme, cx)
                .pr(px(crate::surface_chrome::CONTROL_GAP))
                .border_l_1()
                .border_color(theme.border);
            header = Some(
                div()
                    .w_full()
                    .h(px(crate::surface_chrome::HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(self.render_editor_header(&theme, cx)),
                    )
                    .child(
                        div()
                            .w(px((tree_width * openness - toggle_width).max(0.0)))
                            .h_full()
                            .flex_none()
                            .overflow_hidden()
                            .child(
                                div()
                                    .w(px(tree_width - toggle_width))
                                    .h_full()
                                    .child(tree_header),
                            ),
                    )
                    .child(self.render_tree_toggle(&theme, cx)),
            );

            div()
                .size_full()
                .min_w_0()
                .flex()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.render_preview(window, cx)),
                )
                .child(
                    div()
                        .w(px(tree_width * openness))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(
                            div().size_full().overflow_hidden().child(
                                div()
                                    .w(px(tree_width))
                                    .h_full()
                                    .relative()
                                    .border_l_1()
                                    .border_color(theme.border)
                                    .child(tree_pane),
                            ),
                        )
                        .when(wide && self.preview.tree_sidebar_visible(), |pane| {
                            pane.child(self.preview_split_handle(cx))
                        }),
                )
                .into_any_element()
        } else {
            tree_pane.into_any_element()
        };
        let measured_width = self.preview.width_cell();
        let entity = cx.entity();
        let editor_context_menu = self.render_editor_context_menu(&theme, cx);
        div()
            .id(SharedString::from(format!(
                "files-surface-{}",
                self.chat_id
            )))
            .role(gpui::Role::Group)
            .aria_label("Workspace files")
            .size_full()
            .relative()
            .flex()
            .bg(crate::theme::ink(0.0))
            .when(is_editor, |element| {
                element
                    .on_drag_move(cx.listener(Self::on_preview_split_drag))
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let width = f32::from(bounds.size.width);
                                if (measured_width.get() - width).abs() > 1.0 {
                                    measured_width.set(width);
                                    entity.update(cx, |_, cx| cx.notify());
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
            })
            .flex_col()
            .children(header)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(editor_context_menu)
    }
}

impl FilesSurface {
    pub fn new(
        state: Entity<AppState>,
        chat_id: String,
        autosave_enabled: bool,
        autosave_delay_ms: u64,
        editor_font_size: f32,
        word_wrap: bool,
        show_all_files: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_presentation(
            state,
            chat_id,
            FilesPresentation::Browser,
            None,
            autosave_enabled,
            autosave_delay_ms,
            editor_font_size,
            word_wrap,
            show_all_files,
            cx,
        )
    }

    pub fn new_editor(
        state: Entity<AppState>,
        chat_id: String,
        path: String,
        autosave_enabled: bool,
        autosave_delay_ms: u64,
        editor_font_size: f32,
        word_wrap: bool,
        show_all_files: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_presentation(
            state,
            chat_id,
            FilesPresentation::Editor,
            Some(path),
            autosave_enabled,
            autosave_delay_ms,
            editor_font_size,
            word_wrap,
            show_all_files,
            cx,
        )
    }

    fn new_with_presentation(
        state: Entity<AppState>,
        chat_id: String,
        presentation: FilesPresentation,
        editor_path: Option<String>,
        autosave_enabled: bool,
        autosave_delay_ms: u64,
        editor_font_size: f32,
        word_wrap: bool,
        show_all_files: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::new("Search files", cx)
                .with_accessibility_role(gpui::Role::SearchInput)
                .with_text_metrics(11.0, 16.0)
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => this.on_search_edited(cx),
            ComposerInputEvent::Submitted | ComposerInputEvent::MentionAccept => {
                this.activate_search_result(cx)
            }
            ComposerInputEvent::MentionNavigate(delta) => {
                let len = this.search_state.visible_len();
                if len > 0 {
                    this.search_state.active = if *delta < 0 {
                        this.search_state.active.saturating_sub(1)
                    } else {
                        (this.search_state.active + 1).min(len - 1)
                    };
                    this.search_list
                        .scroll_to_reveal_item(this.search_state.active);
                    cx.notify();
                }
            }
            ComposerInputEvent::MentionDismiss => this.clear_search(cx),
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged => {}
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            if this.sync_target(cx) {
                this.ensure_loaded(cx);
            }
        });
        let mut surface = Self {
            state,
            chat_id,
            review_comment_flush_source: NEXT_REVIEW_COMMENT_FLUSH_SOURCE
                .fetch_add(1, Ordering::Relaxed),
            presentation,
            editor_path: editor_path.clone(),
            request_context: None,
            target_change_pending: false,
            pending_request_context: None,
            tree: FileTreeModel::with_include_ignored(show_all_files),
            tree_list: ListState::new(0, ListAlignment::Top, px(560.0)),
            tree_focus: cx.focus_handle(),
            search,
            search_state: FileSearchState::default(),
            search_list: ListState::new(0, ListAlignment::Top, px(420.0)),
            watch_task: None,
            watch_sequence: None,
            watch_error: None,
            preview: FilePreviewState::new(
                autosave_enabled,
                autosave_delay_ms,
                word_wrap,
                editor_font_size,
            ),
            editor_context_menu: crate::popover::Popup::default(),
            loads: HashMap::new(),
            error: None,
            started: false,
            _observe: observe,
            _search_events: search_events,
        };
        surface.sync_target(cx);
        surface
    }

    pub(in crate::files) fn open_editor_context_menu(
        &mut self,
        editor: Entity<editor::FileEditorState>,
        availability: editor::EditorMenuAvailability,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor_context_menu.open(EditorContextMenu {
            editor,
            position,
            availability,
        });
        cx.notify();
    }

    fn close_editor_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.editor_context_menu.begin_close() {
            crate::popover::reap_popup(cx, |surface: &mut Self| &mut surface.editor_context_menu);
            cx.notify();
        }
    }

    fn dispatch_editor_context_action(
        &mut self,
        editor: Entity<editor::FileEditorState>,
        action: editor::EditorContextAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_editor_context_menu(cx);
        editor::dispatch_context_action(&editor, action, window, cx);
    }

    fn editor_context_menu_row(
        theme: &crate::theme::Theme,
        id: &'static str,
        label: &'static str,
        enabled: bool,
        editor: Entity<editor::FileEditorState>,
        action: editor::EditorContextAction,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        crate::popover::menu_row(theme, false, id)
            .id(id)
            .when(!enabled, |row| row.opacity(0.38).cursor_default())
            .when(enabled, |row| {
                row.on_click(cx.listener(move |this, _, window, cx| {
                    this.dispatch_editor_context_action(editor.clone(), action, window, cx)
                }))
            })
            .child(label)
            .into_any_element()
    }

    fn render_editor_context_menu(
        &mut self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let menu = self.editor_context_menu.get()?;
        let editor = menu.editor.clone();
        let position = menu.position;
        let availability = menu.availability;
        let closing = self.editor_context_menu.closing_since();

        let card = crate::popover::popover_card(theme)
            .w(px(170.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_editor_context_menu(cx)))
            .flex()
            .flex_col()
            .child(Self::editor_context_menu_row(
                theme,
                "files-editor-context-cut",
                "Cut",
                availability.cut,
                editor.clone(),
                editor::EditorContextAction::Cut,
                cx,
            ))
            .child(Self::editor_context_menu_row(
                theme,
                "files-editor-context-copy",
                "Copy",
                availability.copy,
                editor.clone(),
                editor::EditorContextAction::Copy,
                cx,
            ))
            .child(Self::editor_context_menu_row(
                theme,
                "files-editor-context-paste",
                "Paste",
                availability.paste,
                editor.clone(),
                editor::EditorContextAction::Paste,
                cx,
            ))
            .child(crate::popover::menu_separator())
            .child(Self::editor_context_menu_row(
                theme,
                "files-editor-context-select-all",
                "Select All",
                true,
                editor,
                editor::EditorContextAction::SelectAll,
                cx,
            ))
            .into_any_element();

        Some(crate::popover::menu_at(
            "files-editor-context-menu",
            position,
            card,
            closing,
        ))
    }

    pub fn set_autosave_delay_ms(&mut self, delay_ms: u64, cx: &mut Context<Self>) {
        let pending = self.preview.set_autosave_delay_ms(delay_ms);
        for path in pending {
            self.schedule_autosave(path, cx);
        }
        cx.notify();
    }

    pub fn set_autosave_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let pending = self.preview.set_autosave_enabled(enabled);
        for path in pending {
            self.schedule_autosave(path, cx);
        }
        cx.notify();
    }

    pub fn set_word_wrap(
        &mut self,
        word_wrap: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_word_wrap(word_wrap, window, cx);
    }

    pub fn set_editor_font_size(&mut self, editor_font_size: f32, cx: &mut Context<Self>) {
        self.preview.set_editor_font_size(editor_font_size);
        cx.notify();
    }

    pub fn set_show_all_files(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        self.apply_show_all_files(show_all_files, cx);
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        self.sync_target(cx);
        if self.request_context.is_none() {
            return;
        }
        self.ensure_watch(cx);
        if self.presentation.is_editor()
            && !self.preview.has_active()
            && let Some(path) = self.editor_path.clone()
        {
            self.open_file(path, cx);
        }
        if self.started {
            return;
        }
        self.started = true;
        self.load_directory(String::new(), None, cx);
    }

    pub fn retry_root(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.started = true;
        self.load_directory(String::new(), None, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loads.clear();
        self.error = None;
        self.tree.reset();
        self.sync_tree_list();
        self.started = true;
        self.load_directory(String::new(), None, cx);
    }

    pub fn tab_title(&self) -> SharedString {
        self.editor_path
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
            .unwrap_or("Files")
            .into()
    }

    /// The file represented by this surface tab, when the browser has already
    /// promoted itself to an editor.
    pub fn attachment_path(&self) -> Option<&str> {
        self.editor_path.as_deref()
    }

    pub(super) fn open_tree_file(&mut self, path: String, cx: &mut Context<Self>) {
        if self.presentation.is_editor() {
            cx.emit(FilesEvent::OpenFile(path));
            return;
        }

        self.presentation = FilesPresentation::Editor;
        self.editor_path = Some(path.clone());
        self.open_file(path, cx);
        cx.emit(FilesEvent::TitleChanged);
    }

    fn toggle_ignored(&mut self, cx: &mut Context<Self>) {
        cx.emit(FilesEvent::ShowAllFilesChanged(
            !self.tree.include_ignored(),
        ));
    }

    fn apply_show_all_files(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        if self.tree.set_include_ignored(show_all_files) {
            self.loads.clear();
            self.error = None;
            self.sync_tree_list();
            self.started = true;
            self.load_directory(String::new(), None, cx);
            if !self.search_state.query.is_empty() {
                self.search_state.query.clear();
                self.on_search_edited(cx);
            }
        }
    }

    pub fn load_directory(
        &mut self,
        directory: String,
        cursor: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(request_context) = self.request_context.clone() else {
            self.error = Some("No workspace available for this chat.".into());
            cx.notify();
            return;
        };
        let generation = self.tree.generation();
        if !self.tree.begin_load(&directory, cursor.clone(), generation) {
            return;
        }
        self.sync_tree_list();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.tree.fail_load(
                &directory,
                cursor,
                "Workspace service is still starting.",
                generation,
            );
            self.sync_tree_list();
            cx.notify();
            return;
        };
        let key = (directory.clone(), cursor.clone());
        let request = ListWorkspaceDirectoryRequest {
            target: request_context.target.clone(),
            directory: directory.clone(),
            include_ignored: self.tree.include_ignored(),
            cursor: cursor.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, request_context);
        let task = cx.spawn(async move |this, cx| {
            let mut result = client.list_directory(request.clone()).await;
            if result.as_ref().is_err_and(|error| error.retryable()) {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = client.list_directory(request).await;
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.tree.generation() != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        surface.error = None;
                        surface.tree.apply_page(page, generation);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if directory.is_empty() {
                            surface.error = Some(message.clone().into());
                        }
                        surface
                            .tree
                            .fail_load(&directory, cursor, message, generation);
                    }
                }
                surface.sync_tree_list();
                cx.notify();
            });
        });
        self.loads.insert(key, task);
        cx.notify();
    }

    pub fn tree(&self) -> &FileTreeModel {
        &self.tree
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    fn sync_target(&mut self, cx: &mut Context<Self>) -> bool {
        let next = FilesRequestContext::for_chat(self.state.read(cx), &self.chat_id);
        if self.request_context == next {
            self.target_change_pending = false;
            self.pending_request_context = None;
            return false;
        }
        if self.preview.has_unsaved_changes() {
            self.target_change_pending = true;
            self.pending_request_context = next;
            self.preview.cancel_autosaves();
            cx.notify();
            return false;
        }
        self.apply_target(next, cx);
        true
    }

    pub(super) fn apply_pending_target(&mut self, cx: &mut Context<Self>) {
        if !self.target_change_pending {
            return;
        }
        let next = self.pending_request_context.take();
        self.target_change_pending = false;
        self.apply_target(next, cx);
        self.ensure_loaded(cx);
        cx.notify();
    }

    fn apply_target(&mut self, next: Option<FilesRequestContext>, cx: &mut Context<Self>) {
        self.cancel_review_comment_flush(cx);
        self.loads.clear();
        self.watch_task = None;
        self.watch_sequence = None;
        self.watch_error = None;
        self.editor_context_menu = crate::popover::Popup::default();
        self.preview.reset();
        self.tree.reset();
        self.sync_tree_list();
        self.error = if next.is_none() {
            Some("No workspace available for this chat.".into())
        } else {
            None
        };
        self.request_context = next;
        self.started = false;
    }

    fn sync_tree_list(&self) {
        self.tree_list
            .reset_with_uniform_height(self.tree.visible_rows().len(), px(tree::TREE_ROW_HEIGHT));
    }

    fn render_header(&mut self, theme: &crate::theme::Theme, cx: &mut Context<Self>) -> gpui::Div {
        let include_ignored = self.tree.include_ignored();
        toolbar(theme)
            .child(
                div()
                    .h(px(TOOLBAR_BUTTON_SIZE))
                    .min_w_0()
                    .flex_1()
                    .px(px(8.0))
                    .rounded(px(TOOLBAR_BUTTON_RADIUS))
                    .bg(crate::theme::ink(0.035))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(11.5))
                    .child(
                        crate::icons::icon(crate::icons::MAGNIFER)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.text_faint),
                    )
                    .child(div().min_w_0().flex_1().child(self.search.clone())),
            )
            .child(
                toolbar_button(
                    "files-toggle-ignored",
                    if include_ignored {
                        "Hide hidden and ignored files"
                    } else {
                        "Show all files (even hidden)"
                    },
                )
                .when(include_ignored, |element| {
                    element.bg(crate::theme::wash(0.1))
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_ignored(cx)))
                .child(
                    crate::icons::icon(if include_ignored {
                        crate::icons::EYE
                    } else {
                        crate::icons::EYE_CLOSED
                    })
                    .size(px(crate::surface_chrome::ICON_SIZE))
                    .text_color(if include_ignored {
                        theme.text
                    } else {
                        theme.text_muted
                    }),
                ),
            )
    }
}
