use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, HighlightStyle, ListAlignment,
    ListSizingBehavior, ListState, Point, Render, ScrollHandle, SharedString, Subscription, Window,
    div, font, list, prelude::*, px,
};
use gpui_base::input::{RopeExt as _, TextDecoration, TextDecorationCollection};
use zeron_proto::{
    ReadWorkspaceFileRequest, WorkspaceFileSearchMatch, WorkspaceReadOnlyReason,
    WriteWorkspaceFileOutcome, WriteWorkspaceFileRequest,
};

use super::{
    FilesCloseDisposition, FilesEvent, FilesSurface,
    client::{FilesRequestContext, WorkspaceFilesClient},
    document::{DocumentKey, DocumentPhase, FileDocument},
    toolbar, toolbar_button,
};
use crate::{
    comments::{self, ReviewComment},
    composer::{ComposerInput, ComposerInputEvent},
    icons::{self, icon},
    syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache},
    theme::Theme,
};

const PREVIEW_LINE_HEIGHT: f32 = 20.0;
const WIDE_BREAKPOINT: f32 = 680.0;
const TREE_SPLIT_DEFAULT: f32 = 286.0;
const EDITOR_COMMENT_CARD_WIDTH: f32 = 320.0;
const EDITOR_COMMENT_CARD_MARGIN: f32 = 8.0;
const EDITOR_COMMENT_CARD_MIN_ANCHORED_WIDTH: f32 = 220.0;
const EDITOR_COMMENT_DRAFT_HEIGHT: f32 = 92.0;
// A read response is capped at 8 MiB and editable files at 1 MiB. The byte
// budget bounds large previews while the entry cap bounds many tiny editors.
// Protected documents may exceed either limit rather than risk losing work.
const MAX_RETAINED_DOCUMENTS: usize = 16;
const MAX_RETAINED_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

struct HighlightedFile {
    content_hash: String,
    document: Arc<zeron_syntax::HighlightedDocument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentAnchorEdge {
    Start,
    End,
}

struct EditorCommentAnchor {
    range: TextDecorationCollection,
    edge: CommentAnchorEdge,
}

struct EditorCommentDraft {
    key: String,
    path: String,
    line: u32,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

struct EditorOverlayRow {
    line: u32,
    top: f32,
}

struct EditorOverlayLayout {
    gutter_width: f32,
    line_height: f32,
    viewport_width: f32,
    viewport_height: f32,
    rows: Vec<EditorOverlayRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadDecision {
    ReloadNow,
    AwaitDiscardConfirmation,
}

/// Openness is independent of the dragged width, so resizing remains direct.
#[derive(Default)]
struct TreeSidebarMotion {
    target: Option<bool>,
    from: f32,
    started: Option<Instant>,
}

impl TreeSidebarMotion {
    fn sample(&mut self, visible: bool, now: Instant, reduced: bool) -> (f32, bool) {
        let end = f32::from(visible);
        let duration = crate::motion::RESIZE
            .total()
            .mul_f32(crate::motion::speed_scale());
        // Layout and file activation changes are immediate. Only the toggle
        // action starts a transition through animate_to.
        if reduced || self.target != Some(visible) {
            self.target = Some(visible);
            self.started = None;
            return (end, false);
        }
        if let Some(started) = self.started {
            let raw = now.saturating_duration_since(started).as_secs_f32() / duration.as_secs_f32();
            if raw < 1.0 {
                return (
                    crate::motion::lerp(self.from, end, crate::motion::RESIZE.progress(raw)),
                    true,
                );
            }
            self.started = None;
        }
        (end, false)
    }

    fn animate_to(&mut self, previous: bool, visible: bool, now: Instant) {
        self.from = self.sample(previous, now, false).0;
        self.target = Some(visible);
        self.started = Some(now);
    }
}

pub(super) struct FilePreviewState {
    documents: HashMap<String, FileDocument>,
    document_recency: VecDeque<String>,
    active: Option<String>,
    highlights: HashMap<String, HighlightedFile>,
    syntax_cache: SyntaxHighlightCache,
    list: ListState,
    horizontal_scroll: ScrollHandle,
    surface_width: Rc<Cell<f32>>,
    word_wrap: bool,
    editor_font_size: f32,
    autosave_enabled: bool,
    autosave_delay_ms: u64,
    reload_confirmation: Option<String>,
    close_requested: bool,
    tree_sidebar_visible: bool,
    tree_sidebar_dismissed: bool,
    tree_width: f32,
    tree_motion: TreeSidebarMotion,
    comment_anchors: HashMap<String, HashMap<String, EditorCommentAnchor>>,
    comment_draft: Option<EditorCommentDraft>,
    active_comment: Option<String>,
}

impl FilePreviewState {
    pub(super) fn new(
        autosave_enabled: bool,
        autosave_delay_ms: u64,
        word_wrap: bool,
        editor_font_size: f32,
    ) -> Self {
        Self {
            documents: HashMap::new(),
            document_recency: VecDeque::new(),
            active: None,
            highlights: HashMap::new(),
            syntax_cache: SyntaxHighlightCache::default(),
            list: ListState::new(0, ListAlignment::Top, px(520.0)),
            horizontal_scroll: ScrollHandle::new(),
            surface_width: Rc::new(Cell::new(520.0)),
            word_wrap,
            editor_font_size,
            autosave_enabled,
            autosave_delay_ms,
            reload_confirmation: None,
            close_requested: false,
            tree_sidebar_visible: false,
            tree_sidebar_dismissed: false,
            tree_width: TREE_SPLIT_DEFAULT,
            tree_motion: TreeSidebarMotion::default(),
            comment_anchors: HashMap::new(),
            comment_draft: None,
            active_comment: None,
        }
    }

    pub(super) fn reset(&mut self) {
        self.documents.clear();
        self.document_recency.clear();
        self.active = None;
        self.highlights.clear();
        self.list.reset(0);
        self.reload_confirmation = None;
        self.close_requested = false;
        self.tree_sidebar_visible = false;
        self.tree_motion = TreeSidebarMotion::default();
        self.comment_anchors.clear();
        self.comment_draft = None;
        self.active_comment = None;
    }

    pub(super) fn has_active(&self) -> bool {
        self.active.is_some()
    }

    fn touch_document(&mut self, path: &str) {
        self.document_recency.retain(|candidate| candidate != path);
        self.document_recency.push_back(path.to_string());
    }

    fn retained_document_bytes(&self) -> usize {
        let documents = self.documents.values().fold(0usize, |total, document| {
            total.saturating_add(document.estimated_retained_bytes())
        });
        self.highlights
            .values()
            .fold(documents, |total, highlight| {
                total.saturating_add(estimated_highlighted_file_bytes(highlight))
            })
    }

    fn document_is_evictable(&self, path: &str, protected_paths: &HashSet<String>) -> bool {
        if self.active.as_deref() == Some(path)
            || self.reload_confirmation.as_deref() == Some(path)
            || self
                .comment_draft
                .as_ref()
                .is_some_and(|draft| draft.path == path)
            || protected_paths.contains(path)
        {
            return false;
        }
        self.documents.get(path).is_some_and(|document| {
            !document.is_dirty()
                && matches!(
                    document.phase,
                    DocumentPhase::Ready
                        | DocumentPhase::ReadOnly(_)
                        | DocumentPhase::Error(_)
                        | DocumentPhase::DeletedOnDisk
                )
                && document.read_task.is_none()
                && document.highlight_task.is_none()
                && document.autosave_task.is_none()
                && document.save_task.is_none()
                && document.reconcile_task.is_none()
                && document.pending_save.is_none()
                && document.pending_external_reload.is_none()
                && !document.reconcile_after_save
                && !document.review_comment_flush_pending
        })
    }

    fn evict_document(&mut self, path: &str) -> bool {
        if self.documents.remove(path).is_none() {
            return false;
        }
        self.document_recency.retain(|candidate| candidate != path);
        self.highlights.remove(path);
        if let Some(anchors) = self.comment_anchors.remove(path)
            && self
                .active_comment
                .as_ref()
                .is_some_and(|id| anchors.contains_key(id))
        {
            self.active_comment = None;
        }
        true
    }

    fn trim_document_cache(&mut self, protected_paths: &HashSet<String>) -> Vec<String> {
        self.trim_document_cache_to(
            protected_paths,
            MAX_RETAINED_DOCUMENTS,
            MAX_RETAINED_DOCUMENT_BYTES,
        )
    }

    fn trim_document_cache_to(
        &mut self,
        protected_paths: &HashSet<String>,
        max_documents: usize,
        max_bytes: usize,
    ) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.documents.len() > max_documents || self.retained_document_bytes() > max_bytes {
            let candidate = self
                .document_recency
                .iter()
                .find(|path| self.document_is_evictable(path, protected_paths))
                .cloned()
                .or_else(|| {
                    self.documents
                        .keys()
                        .find(|path| self.document_is_evictable(path, protected_paths))
                        .cloned()
                });
            let Some(candidate) = candidate else {
                break;
            };
            if self.evict_document(&candidate) {
                evicted.push(candidate);
            }
        }
        evicted
    }

    pub(super) fn is_wide(&self) -> bool {
        self.surface_width.get() >= WIDE_BREAKPOINT
    }

    pub(super) fn width_cell(&self) -> Rc<Cell<f32>> {
        self.surface_width.clone()
    }

    pub(super) fn tree_sidebar_visible(&self) -> bool {
        self.tree_sidebar_visible || (self.is_wide() && !self.tree_sidebar_dismissed)
    }

    fn show_tree_sidebar(&mut self) {
        self.tree_sidebar_visible = true;
        self.tree_sidebar_dismissed = false;
    }

    fn toggle_tree_sidebar(&mut self) {
        let previous = self.tree_sidebar_visible();
        if previous {
            self.tree_sidebar_visible = false;
            self.tree_sidebar_dismissed = true;
        } else {
            self.show_tree_sidebar();
        }
        self.tree_motion
            .animate_to(previous, self.tree_sidebar_visible(), Instant::now());
    }

    fn word_wrap(&self) -> bool {
        self.word_wrap
    }

    pub(super) fn set_editor_font_size(&mut self, editor_font_size: f32) {
        self.editor_font_size = editor_font_size;
    }

    pub(super) fn set_autosave_delay_ms(&mut self, delay_ms: u64) -> Vec<String> {
        self.autosave_delay_ms = delay_ms;
        let mut pending = Vec::new();
        for (path, document) in &mut self.documents {
            document.autosave_task = None;
            if self.autosave_enabled && document.can_autosave() {
                pending.push(path.clone());
            }
        }
        pending
    }

    pub(super) fn set_autosave_enabled(&mut self, enabled: bool) -> Vec<String> {
        self.autosave_enabled = enabled;
        let mut pending = Vec::new();
        for (path, document) in &mut self.documents {
            document.autosave_task = None;
            if enabled && document.can_autosave() {
                pending.push(path.clone());
            }
        }
        pending
    }

    pub(super) fn tree_sidebar_frame(&mut self, window: &mut Window, cx: &App) -> f32 {
        let (openness, active) = self.tree_motion.sample(
            self.tree_sidebar_visible(),
            Instant::now(),
            crate::motion::reduced_motion(cx),
        );
        if active {
            window.request_animation_frame();
        }
        openness
    }

    pub(super) fn tree_width(&self) -> f32 {
        self.tree_width
    }

    pub(super) fn has_unsaved_changes(&self) -> bool {
        self.documents.values().any(FileDocument::is_dirty)
    }

    pub(super) fn dirty_paths(&self) -> Vec<String> {
        self.documents
            .iter()
            .filter(|(_, document)| document.is_dirty())
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub(super) fn cancel_autosaves(&mut self) {
        for document in self.documents.values_mut() {
            document.autosave_task = None;
        }
    }

    fn request_reload(&mut self, path: &str) -> ReloadDecision {
        let Some(document) = self.documents.get_mut(path) else {
            return ReloadDecision::ReloadNow;
        };
        if !document.is_dirty() {
            self.reload_confirmation = None;
            return ReloadDecision::ReloadNow;
        }

        // A destructive reload must remain pending until the user explicitly
        // confirms it. Pause delayed autosave so it cannot race the choice.
        document.autosave_task = None;
        self.reload_confirmation = Some(path.to_string());
        ReloadDecision::AwaitDiscardConfirmation
    }

    fn autosave_paused_for_reload(&self, path: &str) -> bool {
        self.reload_confirmation.as_deref() == Some(path)
    }

    pub(super) fn narrow_tree_width(&self) -> f32 {
        (self.surface_width.get() * 0.44).clamp(152.0, self.tree_width)
    }
}

fn estimated_highlighted_file_bytes(highlight: &HighlightedFile) -> usize {
    let document = &highlight.document;
    std::mem::size_of::<HighlightedFile>()
        .saturating_add(highlight.content_hash.capacity())
        .saturating_add(
            document
                .lines
                .capacity()
                .saturating_mul(std::mem::size_of::<Vec<zeron_syntax::HighlightSpan>>()),
        )
        .saturating_add(document.lines.iter().fold(0usize, |total, line| {
            total.saturating_add(
                line.capacity()
                    .saturating_mul(std::mem::size_of::<zeron_syntax::HighlightSpan>()),
            )
        }))
}

fn document_key(context: &FilesRequestContext, path: String) -> DocumentKey {
    DocumentKey {
        chat_id: context.target.chat_id.clone().unwrap_or_default(),
        checkout_id: context.checkout_id.clone(),
        path,
    }
}

fn document_blocks_lifecycle(document: &FileDocument) -> bool {
    document.is_dirty()
        && matches!(
            document.phase,
            DocumentPhase::SaveFailed(_)
                | DocumentPhase::Conflict { .. }
                | DocumentPhase::ExternallyModified { .. }
                | DocumentPhase::DeletedOnDisk
        )
}

fn document_finishes_review_comment_flush(document: &FileDocument) -> bool {
    (!document.is_dirty() && matches!(document.phase, DocumentPhase::Ready))
        || matches!(
            document.phase,
            DocumentPhase::SaveFailed(_) | DocumentPhase::Conflict { .. }
        )
}

fn comment_anchor_range(
    text: &gpui_base::input::Rope,
    line: u32,
) -> Option<(std::ops::Range<usize>, CommentAnchorEdge)> {
    let line = line.saturating_sub(1) as usize;
    if line >= text.lines_len() {
        return None;
    }
    let start = text.line_start_offset(line);
    let end = if line + 1 < text.lines_len() {
        text.line_start_offset(line + 1)
    } else {
        text.len()
    };
    if start < end {
        Some((start..end, CommentAnchorEdge::Start))
    } else if start > 0 {
        // The trailing empty line has no byte of its own. Track the newline
        // immediately before it and resolve the anchor from the range's end.
        Some((start - 1..start, CommentAnchorEdge::End))
    } else {
        None
    }
}

fn tracked_comment_line(
    text: &gpui_base::input::Rope,
    range: &std::ops::Range<usize>,
    edge: CommentAnchorEdge,
) -> u32 {
    let offset = match edge {
        CommentAnchorEdge::Start => range.start,
        CommentAnchorEdge::End => range.end,
    }
    .min(text.len());
    (text.offset_to_point(offset).row + 1) as u32
}

fn editor_overlay_layout(editor: &super::editor::FileEditorState) -> Option<EditorOverlayLayout> {
    let visible = editor.visible_row_range()?;
    let input_bounds = editor.input_bounds();
    let text_bounds = editor.text_bounds()?;
    let line_height = f32::from(editor.line_height()?);
    let text = editor.text();
    let mut rows = Vec::with_capacity(visible.len());
    let mut gutter_width = None;
    for line in visible {
        if line >= text.lines_len() {
            continue;
        }
        let offset = text.line_start_offset(line);
        let Some(bounds) = editor.range_to_bounds(&(offset..offset)) else {
            continue;
        };
        gutter_width.get_or_insert_with(|| {
            f32::from(bounds.origin.x - text_bounds.origin.x).clamp(24.0, 64.0)
        });
        rows.push(EditorOverlayRow {
            line: (line + 1) as u32,
            top: f32::from(bounds.origin.y - input_bounds.origin.y),
        });
    }
    Some(EditorOverlayLayout {
        gutter_width: gutter_width.unwrap_or(36.0),
        line_height,
        viewport_width: f32::from(input_bounds.size.width),
        viewport_height: f32::from(input_bounds.size.height),
        rows,
    })
}

fn file_highlight_result_is_current(
    document: &FileDocument,
    document_key: &DocumentKey,
    generation: u64,
    revision: u64,
    content_hash: &str,
) -> bool {
    document.accepts(document_key, generation)
        && document.revision == revision
        && document.content_hash() == Some(content_hash)
}

fn renamed_document_path(path: &str, old_path: &str, new_path: &str) -> Option<String> {
    if path == old_path {
        Some(new_path.to_string())
    } else {
        path.strip_prefix(&format!("{old_path}/"))
            .map(|suffix| format!("{new_path}/{suffix}"))
    }
}

fn path_is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor || path.starts_with(&format!("{ancestor}/"))
}

pub(super) struct PreviewSplitResize;

struct PreviewDragGhost;

impl Render for PreviewDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(1.0))
    }
}

pub(super) struct FileEditorTooltip {
    pub(super) text: SharedString,
}

impl Render for FileEditorTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .max_w(px(360.0))
            .px(px(9.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_overlay)
            .font_family(theme.font_sans.clone())
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .child(self.text.clone())
    }
}

impl FilesSurface {
    pub(crate) fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .preview
            .active
            .as_deref()
            .and_then(|path| self.preview.documents.get(path))
            .and_then(|document| document.editor.clone())
        else {
            return;
        };
        let focus = editor.focus_handle(cx);
        // Tab activation remounts this surface after the click handler returns.
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    pub(super) fn show_tree_sidebar(&mut self, cx: &mut Context<Self>) {
        self.preview.show_tree_sidebar();
        cx.notify();
    }

    fn toggle_tree_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.toggle_tree_sidebar();
        if !self.preview.tree_sidebar_visible() {
            // A hidden search input must not keep receiving editor keystrokes.
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    fn toggle_word_wrap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let word_wrap = !self.preview.word_wrap;
        cx.emit(FilesEvent::WordWrapChanged(word_wrap));
    }

    pub(super) fn apply_word_wrap(
        &mut self,
        word_wrap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview.word_wrap == word_wrap {
            return;
        }
        self.preview.word_wrap = word_wrap;
        self.preview.list.remeasure();
        let editors = self
            .preview
            .documents
            .values()
            .filter_map(|document| document.editor.clone())
            .collect::<Vec<_>>();
        for editor in editors {
            editor.update(cx, |state, cx| state.set_soft_wrap(word_wrap, window, cx));
        }
        cx.notify();
    }

    fn staged_file_comments(&self, path: &str, cx: &App) -> Vec<ReviewComment> {
        self.state
            .read(cx)
            .review_comments(&self.chat_id)
            .iter()
            .filter(|comment| comment.is_file() && comment.path == path)
            .cloned()
            .collect()
    }

    fn trim_document_cache(&mut self, cx: &App) {
        let mut protected_paths = self
            .state
            .read(cx)
            .review_comments(&self.chat_id)
            .iter()
            .filter(|comment| comment.is_file())
            .map(|comment| comment.path.clone())
            .collect::<HashSet<_>>();
        if let Some(path) = self.editor_path.as_ref() {
            protected_paths.insert(path.clone());
        }
        for path in self.preview.trim_document_cache(&protected_paths) {
            tracing::debug!(path = %path, "evicted inactive workspace document");
        }
    }

    fn require_review_comment_flush(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(document) = self.preview.documents.get_mut(path) else {
            return;
        };
        if !document.is_dirty() {
            return;
        }
        document.review_comment_flush_pending = true;
        let key = self.chat_id.clone();
        let source = self.review_comment_flush_source;
        self.state.update(cx, |state, cx| {
            state.begin_review_comment_flush(&key, source);
            cx.notify();
        });
    }

    fn finish_review_comment_flush_if_idle(&mut self, cx: &mut Context<Self>) {
        if self
            .preview
            .documents
            .values()
            .any(|document| document.review_comment_flush_pending)
        {
            return;
        }
        let key = self.chat_id.clone();
        let source = self.review_comment_flush_source;
        self.state.update(cx, |state, cx| {
            state.finish_review_comment_flush(&key, source);
            cx.notify();
        });
    }

    pub(super) fn cancel_review_comment_flush(&mut self, cx: &mut Context<Self>) {
        for document in self.preview.documents.values_mut() {
            document.review_comment_flush_pending = false;
        }
        self.finish_review_comment_flush_if_idle(cx);
    }

    fn sync_editor_comment_anchors(
        &mut self,
        path: &str,
        editor: &Entity<super::editor::FileEditorState>,
        cx: &mut Context<Self>,
    ) {
        let comments = self.staged_file_comments(path, cx);
        let anchors = self
            .preview
            .comment_anchors
            .entry(path.to_string())
            .or_default();
        anchors.retain(|id, _| comments.iter().any(|comment| comment.id == *id));
        let missing = comments
            .into_iter()
            .filter(|comment| !anchors.contains_key(&comment.id))
            .collect::<Vec<_>>();
        for comment in missing {
            let anchor = editor.update(cx, |state, cx| {
                let line = comment.line.min(state.text().lines_len().max(1) as u32);
                let (range, edge) = comment_anchor_range(state.text(), line)?;
                let range = state.create_decorations_collection(
                    vec![TextDecoration::new(range, HighlightStyle::default())],
                    cx,
                );
                Some(EditorCommentAnchor { range, edge })
            });
            if let Some(anchor) = anchor {
                self.preview
                    .comment_anchors
                    .entry(path.to_string())
                    .or_default()
                    .insert(comment.id, anchor);
            }
        }
    }

    fn sync_editor_comment_lines(
        &mut self,
        path: &str,
        editor: &Entity<super::editor::FileEditorState>,
        cx: &mut Context<Self>,
    ) {
        let Some(anchors) = self.preview.comment_anchors.get(path) else {
            return;
        };
        let (updates, detached) = editor.read_with(cx, |state, cx| {
            let mut updates = Vec::new();
            let mut detached = Vec::new();
            for (id, anchor) in anchors {
                if let Some(range) = anchor.range.get_ranges(cx).into_iter().next() {
                    updates.push((
                        id.clone(),
                        tracked_comment_line(state.text(), &range, anchor.edge),
                    ));
                } else {
                    detached.push(id.clone());
                }
            }
            (updates, detached)
        });
        if !detached.is_empty() {
            if let Some(anchors) = self.preview.comment_anchors.get_mut(path) {
                anchors.retain(|id, _| !detached.contains(id));
            }
            self.sync_editor_comment_anchors(path, editor, cx);
        }
        if !updates.is_empty() {
            let key = self.chat_id.clone();
            self.state.update(cx, |state, cx| {
                for (id, line) in updates {
                    state.update_review_comment_line(&key, &id, line);
                }
                cx.notify();
            });
        }
    }

    fn open_editor_comment_draft(
        &mut self,
        path: String,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            ComposerInput::new("Add a comment…", cx)
                .with_text_metrics(12.0, crate::composer::INPUT_LINE_HEIGHT)
        });
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_editor_comment(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let focus = input.read(cx).focus_handle(cx);
        self.preview.comment_draft = Some(EditorCommentDraft {
            key: self.chat_id.clone(),
            path,
            line,
            input,
            _events: events,
        });
        self.preview.active_comment = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel_editor_comment(&mut self, cx: &mut Context<Self>) {
        self.preview.comment_draft = None;
        self.trim_document_cache(cx);
        cx.notify();
    }

    fn commit_editor_comment(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.preview.comment_draft.take() else {
            return;
        };
        let body = draft.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }
        let comment = ReviewComment::file(draft.path.clone(), draft.line, body);
        self.state.update(cx, |state, cx| {
            state.add_review_comment(&draft.key, comment);
            cx.notify();
        });
        if let Some(editor) = self
            .preview
            .documents
            .get(&draft.path)
            .and_then(|document| document.editor.clone())
        {
            self.sync_editor_comment_anchors(&draft.path, &editor, cx);
        }
        self.preview.active_comment = None;
        self.require_review_comment_flush(&draft.path, cx);
        if self.preview.autosave_enabled
            && self
                .preview
                .documents
                .get(&draft.path)
                .is_some_and(FileDocument::can_autosave)
        {
            self.save_document(draft.path, cx);
        }
        cx.notify();
    }

    fn remove_editor_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        let key = self.chat_id.clone();
        let removed_path = self
            .state
            .read(cx)
            .review_comments(&key)
            .iter()
            .find(|comment| comment.id == id && comment.is_file())
            .map(|comment| comment.path.clone());
        self.state.update(cx, |state, cx| {
            state.remove_review_comment(&key, id);
            cx.notify();
        });
        for anchors in self.preview.comment_anchors.values_mut() {
            anchors.remove(id);
        }
        if self.preview.active_comment.as_deref() == Some(id) {
            self.preview.active_comment = None;
        }
        if let Some(path) = removed_path
            && self
                .state
                .read(cx)
                .review_comments(&key)
                .iter()
                .all(|comment| !comment.is_file() || comment.path != path)
            && let Some(document) = self.preview.documents.get_mut(&path)
        {
            document.review_comment_flush_pending = false;
        }
        self.finish_review_comment_flush_if_idle(cx);
        self.trim_document_cache(cx);
        cx.notify();
    }

    fn toggle_editor_comment(&mut self, id: String, cx: &mut Context<Self>) {
        if self.preview.active_comment.as_deref() == Some(id.as_str()) {
            self.preview.active_comment = None;
        } else {
            self.preview.active_comment = Some(id);
            self.preview.comment_draft = None;
        }
        cx.notify();
    }

    pub(super) fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        let leaving_reload_confirmation = self
            .preview
            .reload_confirmation
            .as_deref()
            .is_some_and(|pending| pending != path);
        if leaving_reload_confirmation {
            self.cancel_reload_confirmation(cx);
        }
        self.preview.active = Some(path.clone());
        self.preview.touch_document(&path);
        self.preview.tree_sidebar_visible = false;
        if !self.preview.documents.contains_key(&path) {
            let Some(context) = self.request_context.as_ref() else {
                return;
            };
            self.preview.documents.insert(
                path.clone(),
                FileDocument::loading(document_key(context, path.clone())),
            );
            self.read_file(path, cx);
        } else {
            self.sync_preview_list();
        }
        self.trim_document_cache(cx);
        cx.notify();
    }

    fn read_file(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(document) = self.preview.documents.get_mut(&path) {
                document.phase =
                    DocumentPhase::Error("Workspace service is still starting.".into());
            }
            return;
        };
        let Some(document) = self.preview.documents.get_mut(&path) else {
            return;
        };
        let key = document_key(&context, path.clone());
        document.key = key.clone();
        let generation = document.begin_load();
        let request = ReadWorkspaceFileRequest {
            target: context.target.clone(),
            path: path.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, context.clone());
        let task_path = path.clone();
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut result = client.read_file(request.clone()).await;
            if result.as_ref().is_err_and(|error| error.retryable()) {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                result = client.read_file(request).await;
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.request_context.as_ref() != Some(&context) {
                    return;
                }
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                if !document.accepts(&task_key, generation) {
                    return;
                }
                document.read_task = None;
                match result {
                    Ok(file) => {
                        let highlight = file
                            .text
                            .as_ref()
                            .zip(file.content_hash.as_ref())
                            .map(|(source, hash)| (source.clone(), hash.clone()));
                        document.set_loaded(file);
                        surface.sync_preview_list();
                        if let Some((source, hash)) = highlight {
                            surface.request_file_highlight(task_path.clone(), source, hash, cx);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %task_path, error = %error, "workspace file load failed");
                        document.set_error(error.to_string());
                        surface.sync_preview_list();
                    }
                }
                surface.trim_document_cache(cx);
                cx.notify();
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&key, generation)
        {
            document.read_task = Some(task);
        }
        self.sync_preview_list();
        cx.notify();
    }

    fn request_file_highlight(
        &mut self,
        path: String,
        source: String,
        content_hash: String,
        cx: &mut Context<Self>,
    ) {
        let Some(language) = zeron_syntax::language_for_path(&path) else {
            return;
        };
        let Some((document_key, generation, revision)) = self
            .preview
            .documents
            .get(&path)
            .map(|document| (document.key.clone(), document.generation, document.revision))
        else {
            return;
        };
        let key = DocumentHighlightKey::new(language, &source);
        if let Some(document) = self.preview.syntax_cache.get(&key) {
            if let Some(editor) = self
                .preview
                .documents
                .get(&path)
                .and_then(|file| file.editor.clone())
            {
                super::editor_adapter::install_highlighter(&editor, source, document.clone(), cx);
            }
            self.preview.highlights.insert(
                path,
                HighlightedFile {
                    content_hash,
                    document,
                },
            );
            cx.notify();
            return;
        }
        let highlight_path = path.clone();
        let task_document_key = document_key.clone();
        let source_for_install = source.clone();
        let task = cx.spawn(async move |this, cx| {
            let request_path = highlight_path.clone();
            let highlighted = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                        source: &source,
                        path: Some(&request_path),
                        fence_tag: None,
                    })
                    .ok()
                    .map(Arc::new)
                })
                .await;
            let _ = this.update(cx, |surface, cx| {
                let still_current = surface
                    .preview
                    .documents
                    .get_mut(&highlight_path)
                    .is_some_and(|document| {
                        let current = file_highlight_result_is_current(
                            document,
                            &task_document_key,
                            generation,
                            revision,
                            &content_hash,
                        );
                        if current {
                            document.highlight_task = None;
                        }
                        current
                    });
                if !still_current {
                    return;
                }
                if let Some(document) = highlighted {
                    surface.preview.syntax_cache.insert(key, document.clone());
                    if let Some(editor) = surface
                        .preview
                        .documents
                        .get(&highlight_path)
                        .and_then(|file| file.editor.clone())
                    {
                        super::editor_adapter::install_highlighter(
                            &editor,
                            source_for_install,
                            document.clone(),
                            cx,
                        );
                    }
                    surface.preview.highlights.insert(
                        highlight_path.clone(),
                        HighlightedFile {
                            content_hash,
                            document,
                        },
                    );
                    cx.notify();
                }
                surface.trim_document_cache(cx);
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&document_key, generation)
        {
            document.highlight_task = Some(task);
        }
    }

    pub(super) fn on_editor_change(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(editor) = self
            .preview
            .documents
            .get(path)
            .and_then(|document| document.editor.clone())
        else {
            return;
        };
        let source = editor.read(cx).value().to_string();
        let revision = {
            let Some(document) = self.preview.documents.get_mut(path) else {
                return;
            };
            document.mark_user_edit();
            document.revision
        };
        self.sync_editor_comment_lines(path, &editor, cx);
        if !self.staged_file_comments(path, cx).is_empty() {
            self.require_review_comment_flush(path, cx);
        }
        self.request_editor_highlight(path.to_string(), source, revision, cx);
        self.schedule_autosave(path.to_string(), cx);
        cx.emit(FilesEvent::TitleChanged);
        cx.notify();
    }

    fn request_editor_highlight(
        &mut self,
        path: String,
        source: String,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(language) = zeron_syntax::language_for_path(&path) else {
            return;
        };
        let Some((document_key, generation)) = self
            .preview
            .documents
            .get(&path)
            .map(|document| (document.key.clone(), document.generation))
        else {
            return;
        };
        let cache_key = DocumentHighlightKey::new(language, &source);
        let task_path = path.clone();
        let task_document_key = document_key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let request_path = task_path.clone();
            let source_for_parse = source.clone();
            let highlighted = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                        source: &source_for_parse,
                        path: Some(&request_path),
                        fence_tag: None,
                    })
                    .ok()
                    .map(Arc::new)
                })
                .await;
            let _ = this.update(cx, |surface, cx| {
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                if !document.accepts(&task_document_key, generation)
                    || document.revision != revision
                {
                    return;
                }
                document.highlight_task = None;
                let Some(highlighted) = highlighted else {
                    surface.trim_document_cache(cx);
                    return;
                };
                let editor = document.editor.clone();
                surface
                    .preview
                    .syntax_cache
                    .insert(cache_key, highlighted.clone());
                if let Some(editor) = editor {
                    super::editor_adapter::install_highlighter(&editor, source, highlighted, cx);
                }
                surface.trim_document_cache(cx);
                cx.notify();
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&document_key, generation)
        {
            document.highlight_task = Some(task);
        }
    }

    pub(super) fn schedule_autosave(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.preview.autosave_enabled || self.preview.autosave_paused_for_reload(&path) {
            return;
        }
        let delay = Duration::from_millis(self.preview.autosave_delay_ms);
        let Some((key, generation, revision)) = self
            .preview
            .documents
            .get(&path)
            .filter(|document| document.can_autosave())
            .map(|document| (document.key.clone(), document.generation, document.revision))
        else {
            return;
        };
        let task_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |surface, cx| {
                let still_current = surface.preview.autosave_enabled
                    && !surface.preview.autosave_paused_for_reload(&task_path)
                    && surface
                        .preview
                        .documents
                        .get(&task_path)
                        .is_some_and(|document| {
                            document.accepts(&key, generation)
                                && document.revision == revision
                                && document.can_autosave()
                        });
                if still_current {
                    surface.save_document(task_path, cx);
                }
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path) {
            document.autosave_task = Some(task);
        }
    }

    pub(super) fn save_document(&mut self, path: String, cx: &mut Context<Self>) {
        if self.target_change_pending {
            return;
        }
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(document) = self.preview.documents.get_mut(&path) {
                document.autosave_task = None;
                document.phase =
                    DocumentPhase::SaveFailed("Workspace service is unavailable.".into());
            }
            cx.notify();
            return;
        };
        let Some(editor) = self
            .preview
            .documents
            .get(&path)
            .and_then(|document| document.editor.clone())
        else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let Some((key, generation, pending)) =
            self.preview.documents.get_mut(&path).and_then(|document| {
                let key = document.key.clone();
                let generation = document.generation;
                document
                    .begin_save(text)
                    .map(|pending| (key, generation, pending))
            })
        else {
            return;
        };
        let request = WriteWorkspaceFileRequest {
            expected_checkout_id: pending.expected_checkout_id.clone(),
            target: context.target.clone(),
            path: path.clone(),
            text: pending.text.clone(),
            expected_content_hash: pending.expected_content_hash.clone(),
            encoding: pending.encoding,
            line_ending: pending.line_ending,
        };
        let revision = pending.revision;
        let task_path = path.clone();
        let task_key = key.clone();
        let client = WorkspaceFilesClient::new(engine, context.clone());
        let task = cx.spawn(async move |this, cx| {
            let result = client.write_file(request).await;
            let _ = this.update(cx, |surface, cx| {
                if surface.request_context.as_ref() != Some(&context) {
                    return;
                }
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                if !document.accepts(&task_key, generation) {
                    return;
                }
                match result {
                    Ok(WriteWorkspaceFileOutcome::Written { file }) => {
                        let hash = file.content_hash.clone();
                        if document.finish_save(revision, hash.clone())
                            && let Some(loaded) = document.file.as_mut()
                        {
                            loaded.content_hash = Some(hash);
                            loaded.size = file.size;
                            loaded.modified_at = file.modified_at;
                        }
                    }
                    Ok(WriteWorkspaceFileOutcome::Conflict {
                        current_content_hash,
                        ..
                    }) => {
                        tracing::warn!(path = %task_path, "workspace file save conflicted");
                        document.conflict_save(revision, current_content_hash);
                    }
                    Err(error) => {
                        tracing::warn!(path = %task_path, error = %error, "workspace file save failed");
                        document.fail_save(revision, error.to_string());
                    }
                }
                let save_again = document.can_autosave();
                let reconcile = document.reconcile_after_save;
                document.reconcile_after_save = false;
                if document.review_comment_flush_pending
                    && document_finishes_review_comment_flush(document)
                {
                    document.review_comment_flush_pending = false;
                }
                if save_again {
                    if surface.preview.close_requested || surface.target_change_pending {
                        surface.save_document(task_path.clone(), cx);
                    } else {
                        surface.schedule_autosave(task_path.clone(), cx);
                    }
                }
                if reconcile {
                    surface.reconcile_document(task_path.clone(), cx);
                }
                surface.finish_review_comment_flush_if_idle(cx);
                surface.finish_pending_lifecycle(cx);
                surface.trim_document_cache(cx);
                cx.emit(FilesEvent::TitleChanged);
                cx.notify();
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&key, generation)
        {
            document.save_task = Some(task);
        }
        cx.notify();
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.preview.has_unsaved_changes()
    }

    pub fn prepare_close(&mut self, cx: &mut Context<Self>) -> FilesCloseDisposition {
        let dirty_paths = self.preview.dirty_paths();
        if dirty_paths.is_empty() {
            return FilesCloseDisposition::Allow;
        }
        self.preview.close_requested = true;
        let blocked = dirty_paths.iter().any(|path| {
            self.preview
                .documents
                .get(path)
                .is_some_and(document_blocks_lifecycle)
        });
        for path in dirty_paths {
            if self
                .preview
                .documents
                .get(&path)
                .is_some_and(FileDocument::can_autosave)
            {
                self.save_document(path, cx);
            }
        }
        cx.notify();
        if blocked {
            FilesCloseDisposition::Blocked
        } else {
            FilesCloseDisposition::Pending
        }
    }

    fn retry_pending_close(&mut self, cx: &mut Context<Self>) {
        let paths = self.preview.dirty_paths();
        for path in paths {
            if self
                .preview
                .documents
                .get(&path)
                .is_some_and(FileDocument::can_save)
            {
                self.save_document(path, cx);
            }
        }
    }

    fn keep_open(&mut self, cx: &mut Context<Self>) {
        self.preview.close_requested = false;
        cx.emit(FilesEvent::CloseCancelled);
        cx.notify();
    }

    fn discard_changes_and_close(&mut self, cx: &mut Context<Self>) {
        let closing = self.preview.close_requested;
        for document in self.preview.documents.values_mut() {
            document.discard_changes();
        }
        self.cancel_review_comment_flush(cx);
        self.preview.close_requested = false;
        if closing {
            cx.emit(FilesEvent::CloseReady);
        } else if self.target_change_pending {
            self.apply_pending_target(cx);
        } else {
            cx.emit(FilesEvent::CloseReady);
        }
        cx.emit(FilesEvent::TitleChanged);
        cx.notify();
    }

    fn finish_pending_lifecycle(&mut self, cx: &mut Context<Self>) {
        if self.preview.close_requested && !self.preview.has_unsaved_changes() {
            self.preview.close_requested = false;
            cx.emit(FilesEvent::CloseReady);
        }
        if self.target_change_pending && !self.preview.has_unsaved_changes() {
            self.apply_pending_target(cx);
        }
    }

    pub(super) fn reconcile_document(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(document) = self.preview.documents.get_mut(&path) else {
            return;
        };
        if matches!(document.phase, DocumentPhase::Saving) {
            document.reconcile_after_save = true;
            return;
        }
        let key = document.key.clone();
        let generation = document.generation;
        let request = ReadWorkspaceFileRequest {
            target: context.target.clone(),
            path: path.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, context.clone());
        let task_path = path.clone();
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let started_at = Instant::now();
            let result = client.read_file(request).await;
            tracing::trace!(
                path = %task_path,
                elapsed_ms = started_at.elapsed().as_millis(),
                success = result.is_ok(),
                error = ?result.as_ref().err(),
                "workspace document reconciliation read completed"
            );
            let _ = this.update(cx, |surface, cx| {
                if surface.request_context.as_ref() != Some(&context) {
                    return;
                }
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                if !document.accepts(&task_key, generation) {
                    return;
                }
                if matches!(document.phase, DocumentPhase::Saving) {
                    document.reconcile_after_save = true;
                    return;
                }
                document.reconcile_task = None;
                let Ok(file) = result else {
                    return;
                };
                if file.content_hash == document.saved_hash {
                    let recovered = if matches!(document.phase, DocumentPhase::DeletedOnDisk) {
                        document.restore_on_disk(file);
                        true
                    } else if matches!(document.phase, DocumentPhase::ExternallyModified { .. }) {
                        document.phase = DocumentPhase::Ready;
                        true
                    } else {
                        false
                    };
                    if document.can_autosave() {
                        surface.schedule_autosave(task_path.clone(), cx);
                    }
                    if recovered {
                        cx.notify();
                    }
                    return;
                }
                if document.is_dirty() {
                    document.mark_external(file.content_hash);
                } else {
                    document.queue_external_reload(file);
                }
                cx.notify();
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&key, generation)
        {
            document.reconcile_task = Some(task);
        }
    }

    pub(super) fn reconcile_open_documents(&mut self, cx: &mut Context<Self>) {
        let paths = self.preview.documents.keys().cloned().collect::<Vec<_>>();
        for path in paths {
            self.reconcile_document(path, cx);
        }
    }

    pub(super) fn reconcile_created_documents(&mut self, path: &str, cx: &mut Context<Self>) {
        let paths = self
            .preview
            .documents
            .keys()
            .filter(|document_path| path_is_same_or_descendant(document_path, path))
            .cloned()
            .collect::<Vec<_>>();
        for path in paths {
            self.reconcile_document(path, cx);
        }
    }

    pub(super) fn mark_document_deleted(&mut self, path: &str, cx: &mut Context<Self>) {
        let mut changed = false;
        for (document_path, document) in &mut self.preview.documents {
            if path_is_same_or_descendant(document_path, path) {
                document.mark_deleted();
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn rename_documents(
        &mut self,
        old_path: &str,
        new_path: String,
        cx: &mut Context<Self>,
    ) -> Vec<(String, String)> {
        let renames = self
            .preview
            .documents
            .keys()
            .filter_map(|path| {
                renamed_document_path(path, old_path, &new_path)
                    .map(|renamed| (path.clone(), renamed))
            })
            .collect::<Vec<_>>();
        for (old_document_path, new_document_path) in &renames {
            let Some(mut document) = self.preview.documents.remove(old_document_path) else {
                continue;
            };
            let needs_review =
                document.is_dirty() || matches!(document.phase, DocumentPhase::Saving);
            document.read_task = None;
            document.highlight_task = None;
            document.autosave_task = None;
            document.save_task = None;
            document.reconcile_task = None;
            document.pending_save = None;
            document.key.path = new_document_path.clone();
            if let Some(file) = document.file.as_mut() {
                file.path = new_document_path.clone();
            }
            if let Some(editor) = document.editor.clone() {
                let event_path = new_document_path.clone();
                document.editor_events =
                    Some(super::editor::subscribe_to_changes(&editor, event_path, cx));
            }
            if needs_review {
                document.mark_external(None);
            }
            if self.preview.active.as_deref() == Some(old_document_path) {
                self.preview.active = Some(new_document_path.clone());
            }
            if self.editor_path.as_deref() == Some(old_document_path) {
                self.editor_path = Some(new_document_path.clone());
            }
            if let Some(highlight) = self.preview.highlights.remove(old_document_path) {
                self.preview
                    .highlights
                    .insert(new_document_path.clone(), highlight);
            }
            if let Some(anchors) = self.preview.comment_anchors.remove(old_document_path) {
                self.preview
                    .comment_anchors
                    .insert(new_document_path.clone(), anchors);
            }
            if let Some(draft) = self.preview.comment_draft.as_mut()
                && draft.path == *old_document_path
            {
                draft.path = new_document_path.clone();
            }
            self.preview.document_recency.retain(|recent_path| {
                recent_path != old_document_path && recent_path != new_document_path
            });
            self.preview
                .document_recency
                .push_back(new_document_path.clone());
            let key = self.chat_id.clone();
            self.state.update(cx, |state, _| {
                state.rename_review_comment_path(&key, old_document_path, new_document_path);
            });
            self.preview
                .documents
                .insert(new_document_path.clone(), document);
        }
        if !renames.is_empty() {
            cx.notify();
        }
        renames
    }

    fn apply_pending_external_reload(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .preview
            .documents
            .get_mut(path)
            .and_then(|document| document.pending_external_reload.take())
        else {
            return;
        };
        let text = file.text.clone();
        let hash = file.content_hash.clone();
        let editor = self
            .preview
            .documents
            .get(path)
            .and_then(|document| document.editor.clone());
        if let (Some(editor), Some(text)) = (editor, text.clone()) {
            super::editor::replace_file_contents(&editor, text, window, cx);
        }
        if let Some(document) = self.preview.documents.get_mut(path) {
            document.apply_external_reload(file);
        }
        self.sync_preview_list();
        if let (Some(text), Some(hash)) = (text, hash) {
            self.request_file_highlight(path.to_string(), text, hash, cx);
        }
    }

    fn sync_preview_list(&self) {
        let count = self
            .preview
            .active
            .as_deref()
            .and_then(|path| self.preview.documents.get(path))
            .map(|document| document.lines.len())
            .unwrap_or(0);
        self.preview
            .list
            .reset_with_uniform_height(count, px(PREVIEW_LINE_HEIGHT));
    }

    fn request_reload_active_document(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.preview.active.clone() else {
            return;
        };
        match self.preview.request_reload(&path) {
            ReloadDecision::ReloadNow => self.read_file(path, cx),
            ReloadDecision::AwaitDiscardConfirmation => cx.notify(),
        }
    }

    fn confirm_reload_active_document(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.preview.active.clone() else {
            return;
        };
        if self.preview.reload_confirmation.as_deref() != Some(path.as_str()) {
            return;
        }
        self.preview.reload_confirmation = None;
        self.read_file(path, cx);
    }

    fn cancel_reload_confirmation(&mut self, cx: &mut Context<Self>) {
        let pending = self.preview.reload_confirmation.take();
        if let Some(path) = pending
            && self
                .preview
                .documents
                .get(&path)
                .is_some_and(FileDocument::can_autosave)
        {
            self.schedule_autosave(path, cx);
        }
        cx.notify();
    }

    fn keep_external_edits(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.preview.active.clone() else {
            return;
        };
        if let Some(document) = self.preview.documents.get_mut(&path)
            && let DocumentPhase::ExternallyModified { disk_hash } = &document.phase
        {
            document.phase = DocumentPhase::Conflict {
                disk_hash: disk_hash.clone(),
            };
        }
        self.preview.reload_confirmation = None;
        cx.notify();
    }

    pub fn save_active_document(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.preview.active.clone() {
            self.save_document(path, cx);
        }
    }

    pub(super) fn render_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(active) = self.preview.active.clone() else {
            return gpui::Empty.into_any_element();
        };
        let external = self.preview.documents.get(&active).is_some_and(|document| {
            matches!(
                document.phase,
                DocumentPhase::ExternallyModified { .. } | DocumentPhase::Conflict { .. }
            )
        });
        let confirming_reload = self.preview.reload_confirmation.as_deref() == Some(&active);
        let lifecycle_pending = self.preview.close_requested || self.target_change_pending;
        let lifecycle_blocked = lifecycle_pending
            && self
                .preview
                .documents
                .values()
                .any(document_blocks_lifecycle);
        let body = self.render_document_body(&active, &theme, window, cx);
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(lifecycle_pending, |element| {
                element.child(
                    div()
                        .min_h(px(32.0))
                        .py(px(6.0))
                        .flex_none()
                        .px(px(10.0))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.0))
                        .border_b_1()
                        .border_color(theme.warning.opacity(0.25))
                        .bg(theme.warning.opacity(0.055))
                        .text_size(px(11.0))
                        .text_color(theme.warning_muted)
                        .child(div().min_w_0().max_w_full().whitespace_normal().child(
                            if self.target_change_pending {
                                "Workspace changed. Switch back to save, or discard these edits."
                            } else if lifecycle_blocked {
                                "Changes could not be saved safely."
                            } else {
                                "Saving changes before closing…"
                            },
                        ))
                        .when(lifecycle_blocked || self.target_change_pending, |banner| {
                            banner
                                .child(
                                    div()
                                        .id("files-retry-close-save")
                                        .when(self.target_change_pending, |button| button.hidden())
                                        .ml_auto()
                                        .cursor_pointer()
                                        .text_color(theme.text)
                                        .child("Retry")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.retry_pending_close(cx)
                                        })),
                                )
                                .when(!self.target_change_pending, |banner| {
                                    banner.child(
                                        div()
                                            .id("files-keep-open")
                                            .cursor_pointer()
                                            .text_color(theme.text_muted)
                                            .child("Keep Open")
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.keep_open(cx)),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .id("files-discard-close-changes")
                                        .cursor_pointer()
                                        .text_color(theme.danger_muted)
                                        .child("Discard Changes")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.discard_changes_and_close(cx)
                                        })),
                                )
                        }),
                )
            })
            .when(
                (external || confirming_reload) && !lifecycle_pending,
                |element| {
                    element.child(
                        div()
                            .min_h(px(32.0))
                            .py(px(6.0))
                            .flex_none()
                            .px(px(10.0))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .border_b_1()
                            .border_color(theme.warning.opacity(0.25))
                            .bg(theme.warning.opacity(0.055))
                            .text_size(px(11.0))
                            .text_color(theme.warning_muted)
                            .child(div().min_w_0().max_w_full().whitespace_normal().child(
                                if confirming_reload {
                                    "Discard unsaved changes?"
                                } else {
                                    "This file changed outside Zeron."
                                },
                            ))
                            .child(
                                div()
                                    .ml_auto()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.0))
                                    .when(!confirming_reload, |actions| {
                                        actions
                                            .child(
                                                div()
                                                    .id("files-keep-external-edits")
                                                    .cursor_pointer()
                                                    .text_color(theme.text_muted)
                                                    .child("Keep Editing")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.keep_external_edits(cx)
                                                    })),
                                            )
                                            .child(
                                                div()
                                                    .id("files-reload-external")
                                                    .cursor_pointer()
                                                    .text_color(theme.text)
                                                    .child("Reload from Disk")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.request_reload_active_document(cx)
                                                    })),
                                            )
                                    })
                                    .when(confirming_reload, |actions| {
                                        actions
                                            .child(
                                                div()
                                                    .id("files-cancel-reload")
                                                    .cursor_pointer()
                                                    .text_color(theme.text_muted)
                                                    .child("Cancel")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.cancel_reload_confirmation(cx)
                                                    })),
                                            )
                                            .child(
                                                div()
                                                    .id("files-confirm-reload")
                                                    .cursor_pointer()
                                                    .text_color(theme.text)
                                                    .child("Discard & Reload")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.confirm_reload_active_document(cx)
                                                    })),
                                            )
                                    }),
                            ),
                    )
                },
            )
            .child(body)
            .into_any_element()
    }

    pub(super) fn render_tree_toggle(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        toolbar(theme)
            .w(px(
                crate::surface_chrome::CONTROL_SIZE + crate::surface_chrome::EDGE_INSET
            ))
            .pl_0()
            .child(
                toolbar_button(
                    "files-toggle-tree-sidebar",
                    if self.preview.tree_sidebar_visible() {
                        "Hide files sidebar"
                    } else {
                        "Show files sidebar"
                    },
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_tree_sidebar(window, cx)))
                .child(
                    icon(icons::SIDEBAR_MINIMALISTIC)
                        .size(px(crate::surface_chrome::ICON_SIZE))
                        .text_color(theme.text_muted),
                ),
            )
            .into_any_element()
    }

    pub(super) fn render_editor_header(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let path = self.preview.active.clone()?;
        Some(self.render_breadcrumb(&path, theme, cx))
    }

    fn render_breadcrumb(
        &mut self,
        path: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let markdown = super::markdown_preview::is_markdown(path);
        let showing_markdown = self
            .preview
            .documents
            .get(path)
            .is_some_and(|d| d.show_markdown);
        let parts = path.split('/').collect::<Vec<_>>();
        let reveal_path = path.to_string();
        let tooltip_path: SharedString = path.to_string().into();
        let can_save = !self.target_change_pending
            && self
                .preview
                .documents
                .get(path)
                .is_some_and(FileDocument::can_save);
        let save_status =
            self.preview
                .documents
                .get(path)
                .and_then(|document| match &document.phase {
                    DocumentPhase::SaveFailed(error) => Some((
                        "Save failed",
                        theme.danger_muted,
                        can_save,
                        Some(error.clone()),
                    )),
                    DocumentPhase::Conflict { .. } => Some((
                        "Save conflict",
                        theme.warning_muted,
                        false,
                        Some(SharedString::from(
                            "The file changed on disk. Your editor buffer was preserved.",
                        )),
                    )),
                    DocumentPhase::DeletedOnDisk => Some((
                        "Deleted on disk",
                        theme.warning_muted,
                        false,
                        Some(SharedString::from(
                            "The file was removed on disk. Your editor buffer was preserved.",
                        )),
                    )),
                    DocumentPhase::ExternallyModified { .. } => Some((
                        "Changed on disk",
                        theme.warning_muted,
                        false,
                        Some(SharedString::from(
                            "The file changed on disk. Review it before saving.",
                        )),
                    )),
                    _ => None,
                });
        let mut crumbs = div()
            .id("files-breadcrumb-path")
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .overflow_hidden();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                crumbs = crumbs.child(
                    div()
                        .mx(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint.opacity(0.65))
                        .child("›"),
                );
            }
            crumbs = crumbs.child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_sans.clone())
                    .text_size(px(11.0))
                    .text_color(if index + 1 == parts.len() {
                        theme.text_muted
                    } else {
                        theme.text_faint
                    })
                    .child((*part).to_string()),
            );
        }
        crumbs = crumbs
            .tooltip(move |_, cx| {
                cx.new(|_| FileEditorTooltip {
                    text: tooltip_path.clone(),
                })
                .into()
            })
            .tooltip_show_delay(Duration::from_millis(350));
        toolbar(theme)
            .pr(px(crate::surface_chrome::CONTROL_GAP))
            .child(crumbs)
            .when(markdown, |element| {
                element.child(
                    toolbar_button(
                        "files-toggle-markdown",
                        if showing_markdown {
                            "Show Markdown code"
                        } else {
                            "Preview Markdown"
                        },
                    )
                    .when(showing_markdown, |el| el.bg(crate::theme::wash(0.1)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let Some(path) = this.preview.active.clone() else {
                            return;
                        };
                        if let Some(document) = this.preview.documents.get_mut(&path) {
                            document.show_markdown = !document.show_markdown;
                        }
                        if this
                            .preview
                            .documents
                            .get(&path)
                            .is_some_and(|d| !d.show_markdown)
                        {
                            this.focus_editor(window, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        icon(if showing_markdown {
                            icons::FILE_CODE
                        } else {
                            icons::EYE
                        })
                        .size(px(crate::surface_chrome::ICON_SIZE))
                        .text_color(theme.text_muted),
                    ),
                )
            })
            .when_some(save_status, |element, (label, color, retry, detail)| {
                element.child(
                    div()
                        .id("files-save-status")
                        .h(px(crate::surface_chrome::CONTROL_SIZE))
                        .px(px(6.0))
                        .rounded(px(crate::surface_chrome::CONTROL_RADIUS))
                        .flex()
                        .items_center()
                        .flex_none()
                        .font_family(theme.font_sans.clone())
                        .text_size(px(11.0))
                        .text_color(color)
                        .when(retry, |element| {
                            element
                                .cursor_pointer()
                                .role(gpui::Role::Button)
                                .aria_label("Save file")
                                .hover(|style| style.bg(crate::theme::wash(0.14)))
                                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                                    window.prevent_default()
                                })
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_active_document(cx)),
                                )
                        })
                        .when_some(detail, |element, detail| {
                            element
                                .tooltip(move |_, cx| {
                                    cx.new(|_| FileEditorTooltip {
                                        text: detail.clone(),
                                    })
                                    .into()
                                })
                                .tooltip_show_delay(Duration::from_millis(350))
                        })
                        .child(label),
                )
            })
            .child(
                toolbar_button("files-reveal-active", "Reveal file in tree")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = reveal_path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&reveal_path)
                            .to_string();
                        this.reveal_search_result(
                            WorkspaceFileSearchMatch {
                                path: reveal_path.clone(),
                                name,
                                kind: zeron_proto::WorkspaceEntryKind::File,
                                score: 0,
                            },
                            cx,
                        );
                    }))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(crate::surface_chrome::ICON_SIZE))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                toolbar_button(
                    "files-toggle-word-wrap",
                    if self.preview.word_wrap() {
                        "Disable word wrap"
                    } else {
                        "Enable word wrap"
                    },
                )
                .when(self.preview.word_wrap(), |element| {
                    element.bg(crate::theme::wash(0.1))
                })
                .on_click(cx.listener(|this, _, window, cx| this.toggle_word_wrap(window, cx)))
                .child(
                    icon(icons::LIST)
                        .size(px(crate::surface_chrome::ICON_SIZE))
                        .text_color(if self.preview.word_wrap() {
                            theme.text
                        } else {
                            theme.text_muted
                        }),
                ),
            )
            .into_any_element()
    }

    fn render_document_body(
        &mut self,
        path: &str,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.apply_pending_external_reload(path, window, cx);
        let editor = self.ensure_editor(path, theme, window, cx);
        if self
            .preview
            .documents
            .get(path)
            .is_some_and(|d| d.show_markdown && d.file.as_ref().is_some_and(|f| f.text.is_some()))
        {
            let document = self.preview.documents.get_mut(path).unwrap();
            let version = (
                document.generation,
                document.revision,
                document.loaded_hash.clone(),
            );
            let view = document
                .markdown
                .get_or_insert_with(|| {
                    let owner = cx.weak_entity();
                    cx.new(|cx| {
                        super::markdown_preview::MarkdownPreview::new(
                            path.to_string(),
                            Rc::new(move |path, cx| {
                                let _ = owner
                                    .update(cx, |surface, cx| surface.open_tree_file(path, cx));
                            }),
                            cx,
                        )
                    })
                })
                .clone();
            let media_client = self
                .request_context
                .clone()
                .zip(self.state.read(cx).engine().cloned())
                .map(|(context, engine)| {
                    (
                        WorkspaceFilesClient::new(engine, context),
                        document.file.as_ref().unwrap().checkout_id.clone(),
                    )
                });
            view.update(cx, |view, _| view.media_client = media_client);
            if view.read(cx).version.as_ref() != Some(&version) {
                let source = editor
                    .as_ref()
                    .map(|editor| editor.read(cx).value().to_string())
                    .unwrap_or_else(|| {
                        document
                            .file
                            .as_ref()
                            .and_then(|f| f.text.clone())
                            .unwrap_or_default()
                    });
                let truncated = document.file.as_ref().is_some_and(|f| f.truncated);
                view.update(cx, |view, cx| {
                    view.version = Some(version);
                    view.set_source(source, truncated, cx);
                });
            }
            return view.into_any_element();
        }

        let Some(document) = self.preview.documents.get(path) else {
            return gpui::Empty.into_any_element();
        };
        if matches!(document.phase, DocumentPhase::Loading) {
            return centered_state("Loading file…", theme.text_faint);
        }
        if let DocumentPhase::Error(error) = &document.phase {
            return centered_state(error.clone(), theme.danger_muted);
        }
        if let Some(editor) = editor {
            editor.update(cx, |state, _| {
                state.set_editor_style(super::editor_adapter::editor_style(theme));
            });
            self.sync_editor_comment_anchors(path, &editor, cx);
            let overlays = self.render_editor_comment_overlays(path, &editor, theme, cx);
            return div()
                .id("files-editor-body")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .relative()
                .overflow_hidden()
                .font_family(theme.font_mono.clone())
                .text_size(px(self.preview.editor_font_size))
                .line_height(px(
                    (self.preview.editor_font_size + 8.5).max(PREVIEW_LINE_HEIGHT)
                ))
                .child(super::editor::editor_element(&editor))
                .children(overlays)
                .into_any_element();
        }
        let Some(file) = document.file.as_ref() else {
            return centered_state("This file cannot be previewed.", theme.text_muted);
        };
        if file.text.is_none() {
            return centered_state(read_only_message(file.read_only_reason), theme.text_muted);
        }
        let truncated = file.truncated;
        let word_wrap = self.preview.word_wrap();
        let code_scroll = if word_wrap {
            div()
                .id("files-preview-code-scroll")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .child(
                    list(
                        self.preview.list.clone(),
                        cx.processor(Self::render_preview_line),
                    )
                    .flex_1()
                    .min_h_0()
                    .with_sizing_behavior(ListSizingBehavior::Auto),
                )
        } else {
            let mut scroll = div()
                .id("files-preview-code-scroll")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .overflow_x_scroll()
                .track_scroll(&self.preview.horizontal_scroll)
                .child(
                    div().flex_none().min_w_full().h_full().child(
                        list(
                            self.preview.list.clone(),
                            cx.processor(Self::render_preview_line),
                        )
                        .h_full()
                        .with_sizing_behavior(ListSizingBehavior::Infer),
                    ),
                );
            // GPUI otherwise maps a vertical wheel gesture to X for an x-only
            // scroller, preventing the list from receiving it.
            scroll.style().restrict_scroll_to_axis = Some(true);
            scroll
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .when(truncated, |element| {
                element.child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .px(px(10.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.warning.opacity(0.045))
                        .flex()
                        .items_center()
                        .text_size(px(10.0))
                        .text_color(theme.warning_muted)
                        .child("Large file preview is truncated and read-only."),
                )
            })
            .child(code_scroll)
            .into_any_element()
    }

    fn render_editor_comment_overlays(
        &mut self,
        path: &str,
        editor: &Entity<super::editor::FileEditorState>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(layout) = editor.read_with(cx, |state, _| editor_overlay_layout(state)) else {
            return Vec::new();
        };
        let comments = self.staged_file_comments(path, cx);
        let comments_by_line = comments
            .iter()
            .map(|comment| (comment.line, comment.clone()))
            .collect::<HashMap<_, _>>();
        let (card_left, card_width) = editor_comment_overlay_horizontal(&layout);
        let mut overlays = Vec::with_capacity(layout.rows.len() + 1);
        for row in &layout.rows {
            let group: SharedString = format!("file-comment-gutter-{}-{}", path, row.line).into();
            let cell = div()
                .id(("file-comment-gutter", row.line as usize))
                .absolute()
                .left(px(0.0))
                .top(px(row.top))
                .w(px(layout.gutter_width))
                .h(px(layout.line_height))
                .flex()
                .items_center()
                .justify_center();
            if let Some(comment) = comments_by_line.get(&row.line) {
                let id = comment.id.clone();
                overlays.push(
                    cell.bg(theme.surface_card)
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label(format!("Open comment on line {}", row.line))
                        .hover(|style| style.bg(crate::theme::wash(0.08)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_editor_comment(id.clone(), cx)
                        }))
                        .child(
                            icon(icons::CHAT_ROUND_LINE)
                                .size(px(10.5))
                                .text_color(theme.text_muted),
                        )
                        .into_any_element(),
                );
            } else {
                let target = path.to_string();
                let line = row.line;
                overlays.push(
                    cell.group(group.clone())
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label(format!("Comment on line {line}"))
                        .hover(|style| style.bg(theme.surface_card))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor_comment_draft(target.clone(), line, window, cx)
                        }))
                        .child(
                            div()
                                .size(px(comments::COMMENT_ADDER_SIZE))
                                .opacity(0.0)
                                .group_hover(group, |style| style.opacity(1.0))
                                .rounded(px(4.0))
                                .bg(theme.solid)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(icon(icons::PLUS).size(px(11.0)).text_color(theme.on_solid)),
                        )
                        .into_any_element(),
                );
            }
        }

        let active_comment = self
            .preview
            .active_comment
            .as_deref()
            .and_then(|id| comments.iter().find(|comment| comment.id == id))
            .cloned();
        if let Some(comment) = active_comment
            && let Some(top) = editor_comment_overlay_top(
                &layout,
                comment.line,
                comments::card_height(&comment.body),
            )
        {
            overlays.push(
                self.render_editor_comment_card(comment, card_left, card_width, top, theme, cx),
            );
        } else if let Some(draft) = self
            .preview
            .comment_draft
            .as_ref()
            .filter(|draft| draft.path == path)
            && let Some(top) =
                editor_comment_overlay_top(&layout, draft.line, EDITOR_COMMENT_DRAFT_HEIGHT)
        {
            overlays.push(self.render_editor_comment_draft(
                draft.input.clone(),
                card_left,
                card_width,
                top,
                theme,
                cx,
            ));
        }
        overlays
    }

    fn render_editor_comment_card(
        &self,
        comment: ReviewComment,
        left: f32,
        width: f32,
        top: f32,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let group: SharedString = format!("file-comment-card-{}", comment.id).into();
        let id = comment.id.clone();
        let card = crate::popover::popover_card_flush(theme)
            .group(group.clone())
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .h(px(comments::card_height(&comment.body)))
            .flex()
            .flex_col()
            .font_family(theme.font_sans.clone())
            .px(px(Theme::SPACE_LG))
            .py(px(comments::CARD_PAD_V / 2.0))
            .child(
                div()
                    .h(px(comments::CARD_HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icon(icons::CHAT_ROUND_LINE)
                            .size(px(12.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(comment.location())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "file-comment-remove-{}",
                                comment.id
                            )))
                            .size(px(16.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .opacity(0.0)
                            .group_hover(group, |style| style.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_editor_comment(&id, cx)
                            }))
                            .child(
                                icon(icons::CLOSE_CIRCLE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .text_size(px(12.0))
                    .line_height(px(comments::CARD_LINE_HEIGHT))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(comment.body)),
            );
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
            .into_any_element()
    }

    fn render_editor_comment_draft(
        &self,
        input: Entity<ComposerInput>,
        left: f32,
        width: f32,
        top: f32,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let card = crate::popover::popover_card_flush(theme)
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .h(px(EDITOR_COMMENT_DRAFT_HEIGHT))
            .flex()
            .flex_col()
            .font_family(theme.font_sans.clone())
            .px(px(Theme::SPACE_LG))
            .py(px(8.0))
            .child(
                div()
                    .h(px(48.0))
                    .flex_none()
                    .overflow_hidden()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.input_glass_bg())
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(12.0))
                    .child(input.into_any_element()),
            )
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(6.0))
                    .child(
                        editor_comment_action("file-comment-cancel", "Cancel", false, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_editor_comment(cx))),
                    )
                    .child(
                        editor_comment_action("file-comment-commit", "Comment", true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.commit_editor_comment(cx))),
                    ),
            );
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
            .into_any_element()
    }

    fn ensure_editor(
        &mut self,
        path: &str,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Entity<super::editor::FileEditorState>> {
        let document = self.preview.documents.get(path)?;
        if let Some(editor) = document.editor.clone() {
            return Some(editor);
        }
        if !document.is_editable() {
            return None;
        }
        let text = document.file.as_ref()?.text.clone()?;
        let editor =
            super::editor::new_file_editor(text, path, self.preview.word_wrap, theme, window, cx);
        let event_path = path.to_string();
        let editor_events = super::editor::subscribe_to_changes(&editor, event_path, cx);
        let editor_observer = cx.observe(&editor, |_, _, cx| cx.notify());
        let focus = editor.focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        let document = self.preview.documents.get_mut(path)?;
        document.editor = Some(editor.clone());
        document.editor_events = Some(editor_events);
        document.editor_observer = Some(editor_observer);
        let syntax = self.preview.highlights.get(path).and_then(|highlight| {
            let document = self.preview.documents.get(path)?;
            if document.content_hash() != Some(highlight.content_hash.as_str()) {
                return None;
            }
            Some((
                document.file.as_ref()?.text.clone()?,
                highlight.document.clone(),
            ))
        });
        if let Some((source, highlighted)) = syntax {
            super::editor_adapter::install_highlighter(&editor, source, highlighted, cx);
        }
        self.sync_editor_comment_anchors(path, &editor, cx);
        Some(editor)
    }

    fn render_preview_line(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(path) = self.preview.active.as_deref() else {
            return gpui::Empty.into_any_element();
        };
        let Some(document) = self.preview.documents.get(path) else {
            return gpui::Empty.into_any_element();
        };
        let Some(file) = document.file.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let Some(line) = document.lines.get(index) else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let word_wrap = self.preview.word_wrap();
        let spans = self
            .preview
            .highlights
            .get(path)
            .filter(|highlight| {
                file.content_hash.as_deref() == Some(highlight.content_hash.as_str())
            })
            .and_then(|highlight| highlight.document.lines.get(index))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mono = font(theme.font_mono.clone());
        let runs = crate::markdown::render::runs_for_syntax_line_with_plain(
            line.as_ref(),
            spans,
            &mono,
            theme.text.opacity(0.93),
            &theme,
        );
        div()
            .min_h(px(PREVIEW_LINE_HEIGHT))
            .flex_none()
            .flex()
            .when(word_wrap, |element| element.w_full().items_stretch())
            .when(!word_wrap, |element| {
                element
                    .h(px(PREVIEW_LINE_HEIGHT))
                    .min_w_full()
                    .items_center()
            })
            .child(
                div()
                    .w(px(48.0))
                    .when(!word_wrap, |element| element.h_full())
                    .flex_none()
                    .pr(px(10.0))
                    .border_r_1()
                    .border_color(theme.border.opacity(0.55))
                    .flex()
                    .items_center()
                    .justify_end()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.0))
                    .text_color(theme.text_faint.opacity(0.7))
                    .child((index + 1).to_string()),
            )
            .child(
                div()
                    .when(word_wrap, |element| {
                        element.flex_1().min_w_0().py(px(2.0)).whitespace_normal()
                    })
                    .pl(px(12.0))
                    .pr(px(18.0))
                    .when(!word_wrap, |element| element.whitespace_nowrap())
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .child(gpui::StyledText::new(line.clone()).with_runs(runs)),
            )
            .into_any_element()
    }

    pub(super) fn on_preview_split_drag(
        &mut self,
        event: &gpui::DragMoveEvent<PreviewSplitResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = f32::from(event.bounds.right() - event.event.position.x);
        self.preview.tree_width = width.clamp(220.0, 360.0);
        cx.notify();
    }

    pub(super) fn preview_split_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let color = Theme::of(cx).border_strong;
        div()
            .id("files-preview-split")
            .absolute()
            .left(px(-3.0))
            .top_0()
            .bottom_0()
            .w(px(6.0))
            .occlude()
            .cursor_col_resize()
            .hover(move |style| style.bg(color))
            .on_drag(
                PreviewSplitResize,
                |_, _point: Point<gpui::Pixels>, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| PreviewDragGhost)
                },
            )
            .into_any_element()
    }
}

fn editor_comment_overlay_top(
    layout: &EditorOverlayLayout,
    line: u32,
    card_height: f32,
) -> Option<f32> {
    let row = layout.rows.iter().find(|row| row.line == line)?;
    let preferred = row.top + layout.line_height;
    Some(preferred.clamp(0.0, (layout.viewport_height - card_height).max(0.0)))
}

fn editor_comment_overlay_horizontal(layout: &EditorOverlayLayout) -> (f32, f32) {
    let anchored_left =
        (layout.gutter_width - EDITOR_COMMENT_CARD_MARGIN).max(EDITOR_COMMENT_CARD_MARGIN);
    let anchored_width = (layout.viewport_width - anchored_left - EDITOR_COMMENT_CARD_MARGIN)
        .min(EDITOR_COMMENT_CARD_WIDTH)
        .max(0.0);
    if anchored_width >= EDITOR_COMMENT_CARD_MIN_ANCHORED_WIDTH {
        (anchored_left, anchored_width)
    } else {
        (
            EDITOR_COMMENT_CARD_MARGIN,
            (layout.viewport_width - EDITOR_COMMENT_CARD_MARGIN * 2.0).max(0.0),
        )
    }
}

fn editor_comment_action(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |element| {
            element.bg(theme.solid).text_color(theme.on_solid)
        })
        .when(!primary, |element| {
            element
                .text_color(crate::motion::hover_blend(id, theme.text_muted, theme.text))
                .bg(crate::motion::hover_blend(
                    id,
                    gpui::transparent_black(),
                    theme.element_hover,
                ))
                .on_hover(crate::motion::hover_listener(id))
        })
        .child(SharedString::from(label))
}

fn centered_state(message: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(24.0))
        .text_center()
        .text_size(px(11.5))
        .text_color(color)
        .child(message.into())
        .into_any_element()
}

fn read_only_message(reason: Option<WorkspaceReadOnlyReason>) -> SharedString {
    match reason {
        Some(WorkspaceReadOnlyReason::Binary) => "Binary files cannot be previewed.",
        Some(WorkspaceReadOnlyReason::UnsupportedEncoding) => {
            "This file encoding is not supported."
        }
        Some(WorkspaceReadOnlyReason::Symlink) => "Symlink targets are read-only.",
        Some(WorkspaceReadOnlyReason::PermissionDenied) => "Permission denied.",
        Some(WorkspaceReadOnlyReason::TooLarge) => "This file is too large to preview.",
        Some(WorkspaceReadOnlyReason::MixedLineEndings) => {
            "Files with mixed line endings are read-only."
        }
        Some(WorkspaceReadOnlyReason::NotRegularFile) | None => "This file cannot be previewed.",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_document(path: &str, text: &str) -> FileDocument {
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        });
        document.phase = DocumentPhase::Ready;
        document.lines = Arc::new(vec![SharedString::from(text.to_string())]);
        document
    }

    #[test]
    fn read_only_reasons_have_specific_messages() {
        assert!(read_only_message(Some(WorkspaceReadOnlyReason::Binary)).contains("Binary"));
        assert!(
            read_only_message(Some(WorkspaceReadOnlyReason::UnsupportedEncoding))
                .contains("encoding")
        );
    }

    #[test]
    fn reset_drops_documents_and_active_preview_from_the_previous_target() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        preview.active = Some("private.env".into());
        preview.documents.insert(
            "private.env".into(),
            FileDocument::loading(DocumentKey {
                chat_id: "chat-1".into(),
                checkout_id: Some("checkout-1".into()),
                path: "private.env".into(),
            }),
        );
        preview
            .documents
            .get_mut("private.env")
            .unwrap()
            .mark_external(None);
        preview.tree_sidebar_visible = true;

        preview.reset();

        assert!(preview.documents.is_empty());
        assert!(preview.active.is_none());
        assert!(!preview.tree_sidebar_visible);
    }

    #[test]
    fn document_cache_evicts_the_oldest_safe_entries() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        for index in 0..18 {
            let path = format!("src/{index}.rs");
            preview
                .documents
                .insert(path.clone(), cached_document(&path, "fn main() {}"));
            preview.touch_document(&path);
        }
        preview.active = Some("src/0.rs".into());
        preview.documents.get_mut("src/1.rs").unwrap().revision = 1;
        preview.documents.get_mut("src/2.rs").unwrap().phase =
            DocumentPhase::Conflict { disk_hash: None };
        preview
            .documents
            .get_mut("src/3.rs")
            .unwrap()
            .review_comment_flush_pending = true;
        preview.documents.get_mut("src/7.rs").unwrap().phase = DocumentPhase::Loading;
        let protected = HashSet::from(["src/4.rs".to_string()]);

        let evicted = preview.trim_document_cache_to(&protected, 15, usize::MAX);

        assert_eq!(evicted, vec!["src/5.rs", "src/6.rs", "src/8.rs"]);
        for retained in ["src/0.rs", "src/1.rs", "src/2.rs", "src/3.rs", "src/4.rs"] {
            assert!(preview.documents.contains_key(retained));
        }
        assert!(preview.documents.contains_key("src/7.rs"));
    }

    #[test]
    fn document_cache_uses_retained_bytes_not_only_entry_count() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        for path in ["old.rs", "active.rs"] {
            preview
                .documents
                .insert(path.into(), cached_document(path, &"x".repeat(4 * 1024)));
            preview.touch_document(path);
        }
        preview.active = Some("active.rs".into());
        let active_bytes = preview.documents["active.rs"].estimated_retained_bytes();
        let total_bytes = preview.retained_document_bytes();

        let evicted = preview.trim_document_cache_to(
            &HashSet::new(),
            usize::MAX,
            total_bytes.saturating_sub(1),
        );

        assert_eq!(evicted, vec!["old.rs"]);
        assert!(preview.retained_document_bytes() <= active_bytes);
    }

    #[test]
    fn document_eviction_cleans_path_scoped_companion_state() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        for path in ["old.rs", "active.rs"] {
            preview
                .documents
                .insert(path.into(), cached_document(path, "fn main() {}"));
            preview.touch_document(path);
        }
        preview.active = Some("active.rs".into());
        preview
            .comment_anchors
            .insert("old.rs".into(), HashMap::new());
        let highlighted = Arc::new(
            zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                source: "fn main() {}",
                path: Some("old.rs"),
                fence_tag: None,
            })
            .unwrap(),
        );
        preview.highlights.insert(
            "old.rs".into(),
            HighlightedFile {
                content_hash: "hash".into(),
                document: highlighted,
            },
        );

        let evicted = preview.trim_document_cache_to(&HashSet::new(), 1, usize::MAX);

        assert_eq!(evicted, vec!["old.rs"]);
        assert!(!preview.highlights.contains_key("old.rs"));
        assert!(!preview.comment_anchors.contains_key("old.rs"));
        assert!(!preview.document_recency.iter().any(|path| path == "old.rs"));
    }

    #[test]
    fn reset_preserves_global_editor_preferences() {
        let mut preview = FilePreviewState::new(false, 900, true, 15.0);

        preview.reset();

        assert!(!preview.autosave_enabled);
        assert!(preview.word_wrap());
        assert_eq!(preview.editor_font_size, 15.0);
    }

    #[test]
    fn file_highlight_completion_rejects_a_result_after_a_user_edit() {
        let path = "src/lib.rs";
        let disk_hash = "disk-hash-1";
        let stale_source = "ab";
        let updated_source = "é";
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        });
        document.set_loaded(zeron_proto::WorkspaceFileText {
            checkout_id: "checkout-1".into(),
            path: path.into(),
            text: Some(stale_source.into()),
            content_hash: Some(disk_hash.into()),
            size: stale_source.len() as u64,
            modified_at: None,
            encoding: zeron_proto::WorkspaceTextEncoding::Utf8,
            line_ending: Some(zeron_proto::WorkspaceLineEnding::Lf),
            read_only_reason: None,
            truncated: false,
        });
        let task_document_key = document.key.clone();
        let task_generation = document.generation;
        let task_revision = document.revision;
        let stale_highlight_key =
            DocumentHighlightKey::new(zeron_syntax::LanguageId::Rust, stale_source);

        assert!(file_highlight_result_is_current(
            &document,
            &task_document_key,
            task_generation,
            task_revision,
            disk_hash,
        ));

        document.mark_user_edit();
        let current_highlight_key =
            DocumentHighlightKey::new(zeron_syntax::LanguageId::Rust, updated_source);

        assert_ne!(document.revision, task_revision);
        assert_ne!(current_highlight_key, stale_highlight_key);
        assert!(
            !file_highlight_result_is_current(
                &document,
                &task_document_key,
                task_generation,
                task_revision,
                disk_hash,
            ),
            "highlight for revision {task_revision} was accepted at revision {} after the editor source changed from {stale_source:?} to {updated_source:?}",
            document.revision
        );
    }

    #[test]
    fn autosave_is_opt_in_and_enabling_schedules_dirty_documents() {
        let path = "src/lib.rs";
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        });
        document.set_loaded(zeron_proto::WorkspaceFileText {
            checkout_id: "checkout-1".into(),
            path: path.into(),
            text: Some("fn main() {}".into()),
            content_hash: Some("hash-1".into()),
            size: 12,
            modified_at: None,
            encoding: zeron_proto::WorkspaceTextEncoding::Utf8,
            line_ending: Some(zeron_proto::WorkspaceLineEnding::Lf),
            read_only_reason: None,
            truncated: false,
        });
        document.revision = 1;
        preview.documents.insert(path.into(), document);

        assert!(preview.set_autosave_delay_ms(600).is_empty());
        assert_eq!(preview.set_autosave_enabled(true), vec![path.to_string()]);
        assert!(preview.set_autosave_enabled(false).is_empty());
    }

    #[test]
    fn sidebar_layout_changes_are_immediate_without_a_user_toggle() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        let now = Instant::now();
        assert_eq!(
            preview
                .tree_motion
                .sample(preview.tree_sidebar_visible(), now, false),
            (0.0, false)
        );
        // A newly opened surface measures its width after the first render.
        preview.surface_width.set(WIDE_BREAKPOINT);
        assert_eq!(
            preview
                .tree_motion
                .sample(preview.tree_sidebar_visible(), now, false),
            (1.0, false)
        );
        preview.surface_width.set(WIDE_BREAKPOINT - 1.0);
        assert_eq!(
            preview
                .tree_motion
                .sample(preview.tree_sidebar_visible(), now, false),
            (0.0, false)
        );
        preview.show_tree_sidebar();
        assert_eq!(
            preview
                .tree_motion
                .sample(preview.tree_sidebar_visible(), now, false),
            (1.0, false)
        );
        preview.toggle_tree_sidebar();
        let started = preview.tree_motion.started.unwrap();
        assert_eq!(
            preview
                .tree_motion
                .sample(preview.tree_sidebar_visible(), started, false),
            (1.0, true)
        );
    }

    #[test]
    fn sidebar_motion_reverses_from_its_current_width() {
        let mut motion = TreeSidebarMotion::default();
        let now = Instant::now();
        assert_eq!(motion.sample(true, now, false), (1.0, false));
        motion.animate_to(true, false, now);
        assert_eq!(motion.sample(false, now, false), (1.0, true));
        let midway = now
            + crate::motion::RESIZE
                .total()
                .mul_f32(crate::motion::speed_scale() * 0.4);
        let closing = motion.sample(false, midway, false).0;
        assert!(closing > 0.0 && closing < 1.0);
        motion.animate_to(false, true, midway);
        assert_eq!(motion.sample(true, midway, false), (closing, true));
        assert_eq!(
            motion.sample(true, midway + Duration::from_secs(10), false),
            (1.0, false)
        );
        motion.animate_to(true, false, midway + Duration::from_secs(10));
        assert_eq!(
            motion.sample(false, midway + Duration::from_secs(20), false),
            (0.0, false)
        );
    }

    #[test]
    fn sidebar_motion_snaps_when_reduced_motion_is_enabled() {
        let mut motion = TreeSidebarMotion::default();
        let now = Instant::now();
        motion.sample(true, now, false);
        motion.animate_to(true, false, now);
        assert_eq!(motion.sample(false, now, true), (0.0, false));
        assert_eq!(motion.sample(true, now, true), (1.0, false));
        assert_eq!(motion.sample(true, now, false), (1.0, false));
    }

    #[test]
    fn wide_layout_respects_an_explicitly_hidden_tree_sidebar() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        preview.surface_width.set(WIDE_BREAKPOINT);

        assert!(preview.tree_sidebar_visible());

        preview.toggle_tree_sidebar();
        assert!(!preview.tree_sidebar_visible());

        preview.surface_width.set(WIDE_BREAKPOINT - 1.0);
        preview.surface_width.set(WIDE_BREAKPOINT);
        assert!(!preview.tree_sidebar_visible());
    }

    #[test]
    fn explicitly_showing_tree_sidebar_clears_responsive_dismissal() {
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        preview.surface_width.set(WIDE_BREAKPOINT);
        preview.toggle_tree_sidebar();

        preview.show_tree_sidebar();

        assert!(preview.tree_sidebar_visible());
        preview.tree_sidebar_visible = false;
        assert!(preview.tree_sidebar_visible());
    }

    #[test]
    fn dirty_reload_waits_for_explicit_discard_confirmation() {
        let path = "src/lib.rs";
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        });
        document.revision = 1;
        preview.documents.insert(path.into(), document);

        assert_eq!(
            preview.request_reload(path),
            ReloadDecision::AwaitDiscardConfirmation
        );
        assert_eq!(preview.reload_confirmation.as_deref(), Some(path));
        assert!(preview.documents[path].is_dirty());

        // Repeated toolbar clicks must not bypass the explicit confirmation.
        assert_eq!(
            preview.request_reload(path),
            ReloadDecision::AwaitDiscardConfirmation
        );
    }

    #[test]
    fn clean_reload_proceeds_without_confirmation() {
        let path = "src/lib.rs";
        let mut preview = FilePreviewState::new(false, 900, false, 11.5);
        preview.documents.insert(
            path.into(),
            FileDocument::loading(DocumentKey {
                chat_id: "chat-1".into(),
                checkout_id: Some("checkout-1".into()),
                path: path.into(),
            }),
        );
        preview.reload_confirmation = Some("stale.rs".into());

        assert_eq!(preview.request_reload(path), ReloadDecision::ReloadNow);
        assert!(preview.reload_confirmation.is_none());
    }

    #[test]
    fn failed_save_blocks_lifecycle_until_explicit_recovery() {
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: "src/lib.rs".into(),
        });
        document.revision = 1;
        document.phase = DocumentPhase::SaveFailed("offline".into());

        assert!(document_blocks_lifecycle(&document));

        document.phase = DocumentPhase::Ready;
        assert!(!document_blocks_lifecycle(&document));
    }

    #[test]
    fn terminal_save_outcomes_release_review_comment_flushes() {
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: "src/lib.rs".into(),
        });
        document.revision = 1;
        document.review_comment_flush_pending = true;

        document.phase = DocumentPhase::Saving;
        assert!(!document_finishes_review_comment_flush(&document));

        document.phase = DocumentPhase::SaveFailed("offline".into());
        assert!(document_finishes_review_comment_flush(&document));

        document.phase = DocumentPhase::Conflict { disk_hash: None };
        assert!(document_finishes_review_comment_flush(&document));

        document.phase = DocumentPhase::Ready;
        assert!(!document_finishes_review_comment_flush(&document));

        document.saved_revision = document.revision;
        assert!(document_finishes_review_comment_flush(&document));
    }

    #[test]
    fn directory_rename_updates_open_descendant_paths() {
        assert_eq!(
            renamed_document_path("src/files/mod.rs", "src", "crates/ui/src"),
            Some("crates/ui/src/files/mod.rs".into())
        );
        assert_eq!(
            renamed_document_path("src-old/lib.rs", "src", "crates/ui/src"),
            None
        );
    }

    #[test]
    fn document_paths_match_recreated_files_and_directory_descendants() {
        assert!(path_is_same_or_descendant("src/lib.rs", "src/lib.rs"));
        assert!(path_is_same_or_descendant("src/files/mod.rs", "src"));
        assert!(!path_is_same_or_descendant("src-old/lib.rs", "src"));
    }

    #[test]
    fn comment_ranges_anchor_normal_and_trailing_empty_lines() {
        let text = gpui_base::input::Rope::from("one\ntwo\n");
        let (second, second_edge) = comment_anchor_range(&text, 2).unwrap();
        assert_eq!(second, 4..8);
        assert_eq!(second_edge, CommentAnchorEdge::Start);
        assert_eq!(tracked_comment_line(&text, &second, second_edge), 2);

        let (trailing, trailing_edge) = comment_anchor_range(&text, 3).unwrap();
        assert_eq!(trailing, 7..8);
        assert_eq!(trailing_edge, CommentAnchorEdge::End);
        assert_eq!(tracked_comment_line(&text, &trailing, trailing_edge), 3);
    }

    #[test]
    fn comment_overlay_stays_inside_the_editor_viewport() {
        let layout = EditorOverlayLayout {
            gutter_width: 32.0,
            line_height: 20.0,
            viewport_width: 420.0,
            viewport_height: 100.0,
            rows: vec![EditorOverlayRow { line: 5, top: 80.0 }],
        };
        assert_eq!(editor_comment_overlay_top(&layout, 5, 60.0), Some(40.0));
        assert_eq!(editor_comment_overlay_top(&layout, 6, 60.0), None);
    }

    #[test]
    fn comment_overlay_is_compact_when_space_allows_and_inset_when_narrow() {
        let mut layout = EditorOverlayLayout {
            gutter_width: 40.0,
            line_height: 20.0,
            viewport_width: 480.0,
            viewport_height: 300.0,
            rows: Vec::new(),
        };
        assert_eq!(editor_comment_overlay_horizontal(&layout), (32.0, 320.0));

        layout.viewport_width = 210.0;
        assert_eq!(editor_comment_overlay_horizontal(&layout), (8.0, 194.0));
    }
}

#[cfg(test)]
impl FilesSurface {
    pub(crate) fn seed_pending_exit_test_document(&mut self, failed: bool) {
        let mut document = FileDocument::loading(DocumentKey {
            chat_id: "test".into(),
            checkout_id: None,
            path: "test.rs".into(),
        });
        document.revision = 1;
        document.phase = if failed {
            DocumentPhase::SaveFailed("offline".into())
        } else {
            DocumentPhase::Saving
        };
        self.preview.documents.insert("test.rs".into(), document);
    }
}
