//! Lifecycle and optimistic-concurrency state for an open workspace file.

use std::sync::Arc;

use gpui::{Entity, SharedString, Subscription, Task};
use gpui_base::input::EditorState;
use zeron_proto::{
    WorkspaceFileText, WorkspaceLineEnding, WorkspaceReadOnlyReason, WorkspaceTextEncoding,
    WorkspaceWritableEncoding, WorkspaceWritableLineEnding,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct DocumentKey {
    pub chat_id: String,
    pub checkout_id: Option<String>,
    pub path: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DocumentPhase {
    Loading,
    Ready,
    Saving,
    SaveFailed(SharedString),
    ExternallyModified { disk_hash: Option<String> },
    Conflict { disk_hash: Option<String> },
    DeletedOnDisk,
    ReadOnly(WorkspaceReadOnlyReason),
    Error(SharedString),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingSave {
    pub revision: u64,
    pub text: String,
    pub expected_content_hash: String,
    pub expected_checkout_id: String,
    pub encoding: WorkspaceWritableEncoding,
    pub line_ending: WorkspaceWritableLineEnding,
}

#[allow(dead_code)]
pub(super) struct FileDocument {
    pub markdown: Option<Entity<super::markdown_preview::MarkdownPreview>>,
    pub show_markdown: bool,
    pub key: DocumentKey,
    pub generation: u64,
    pub phase: DocumentPhase,
    pub file: Option<WorkspaceFileText>,
    pub lines: Arc<Vec<SharedString>>,
    pub editor: Option<Entity<EditorState>>,
    pub editor_events: Option<Subscription>,
    pub editor_observer: Option<Subscription>,
    pub loaded_hash: Option<String>,
    pub saved_hash: Option<String>,
    pub revision: u64,
    pub saved_revision: u64,
    pub encoding: Option<WorkspaceWritableEncoding>,
    pub line_ending: Option<WorkspaceWritableLineEnding>,
    pub read_task: Option<Task<()>>,
    pub highlight_task: Option<Task<()>>,
    pub autosave_task: Option<Task<()>>,
    pub save_task: Option<Task<()>>,
    pub reconcile_task: Option<Task<()>>,
    pub pending_save: Option<PendingSave>,
    pub pending_external_reload: Option<WorkspaceFileText>,
    pub reconcile_after_save: bool,
    pub review_comment_flush_pending: bool,
}

#[allow(dead_code)]
impl FileDocument {
    pub fn loading(key: DocumentKey) -> Self {
        Self {
            markdown: None,
            show_markdown: false,
            key,
            generation: 1,
            phase: DocumentPhase::Loading,
            file: None,
            lines: Arc::new(Vec::new()),
            editor: None,
            editor_events: None,
            editor_observer: None,
            loaded_hash: None,
            saved_hash: None,
            revision: 0,
            saved_revision: 0,
            encoding: None,
            line_ending: None,
            read_task: None,
            highlight_task: None,
            autosave_task: None,
            save_task: None,
            reconcile_task: None,
            pending_save: None,
            pending_external_reload: None,
            reconcile_after_save: false,
            review_comment_flush_pending: false,
        }
    }

    pub fn begin_load(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.phase = DocumentPhase::Loading;
        self.read_task = None;
        self.highlight_task = None;
        self.autosave_task = None;
        self.save_task = None;
        self.reconcile_task = None;
        self.pending_save = None;
        self.pending_external_reload = None;
        self.reconcile_after_save = false;
        self.review_comment_flush_pending = false;
        self.generation
    }

    pub fn accepts(&self, key: &DocumentKey, generation: u64) -> bool {
        self.key == *key && self.generation == generation
    }

    pub fn set_loaded(&mut self, file: WorkspaceFileText) {
        let hash = file.content_hash.clone();
        self.editor = None;
        self.editor_events = None;
        self.editor_observer = None;
        self.encoding = writable_encoding(file.encoding);
        self.line_ending = writable_line_ending(file.line_ending);
        self.lines = Arc::new(
            file.text
                .as_deref()
                .unwrap_or_default()
                .split('\n')
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
        );
        self.phase = read_only_reason(&file)
            .map(DocumentPhase::ReadOnly)
            .unwrap_or(DocumentPhase::Ready);
        self.loaded_hash = hash.clone();
        self.saved_hash = hash;
        self.revision = 0;
        self.saved_revision = 0;
        self.pending_save = None;
        self.pending_external_reload = None;
        self.reconcile_after_save = false;
        self.review_comment_flush_pending = false;
        self.file = Some(file);
    }

    pub fn set_error(&mut self, error: impl Into<SharedString>) {
        self.phase = DocumentPhase::Error(error.into());
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn is_editable(&self) -> bool {
        !matches!(
            self.phase,
            DocumentPhase::Loading | DocumentPhase::ReadOnly(_) | DocumentPhase::Error(_)
        ) && self.file.as_ref().is_some_and(|file| file.text.is_some())
            && self.saved_hash.is_some()
            && self.encoding.is_some()
            && self.line_ending.is_some()
    }

    pub fn can_save(&self) -> bool {
        self.is_editable()
            && self.is_dirty()
            && self.pending_save.is_none()
            && matches!(
                self.phase,
                DocumentPhase::Ready | DocumentPhase::SaveFailed(_)
            )
    }

    pub fn can_autosave(&self) -> bool {
        self.can_save() && matches!(self.phase, DocumentPhase::Ready)
    }

    pub fn mark_user_edit(&mut self) {
        if self.is_editable() {
            self.revision = self.revision.wrapping_add(1);
            if matches!(self.phase, DocumentPhase::SaveFailed(_)) {
                self.phase = DocumentPhase::Ready;
            }
        }
    }

    pub fn begin_save(&mut self, text: String) -> Option<PendingSave> {
        if !self.can_save() {
            return None;
        }
        let pending = PendingSave {
            revision: self.revision,
            text,
            expected_content_hash: self.saved_hash.clone()?,
            expected_checkout_id: self.file.as_ref()?.checkout_id.clone(),
            encoding: self.encoding?,
            line_ending: self.line_ending?,
        };
        self.phase = DocumentPhase::Saving;
        self.autosave_task = None;
        self.pending_save = Some(pending.clone());
        Some(pending)
    }

    pub fn finish_save(&mut self, revision: u64, content_hash: String) -> bool {
        if self.pending_save.as_ref().map(|save| save.revision) != Some(revision) {
            return false;
        }
        self.saved_hash = Some(content_hash.clone());
        self.loaded_hash = Some(content_hash);
        self.saved_revision = revision;
        self.pending_save = None;
        self.save_task = None;
        self.phase = DocumentPhase::Ready;
        true
    }

    pub fn fail_save(&mut self, revision: u64, error: impl Into<SharedString>) -> bool {
        if self.pending_save.as_ref().map(|save| save.revision) != Some(revision) {
            return false;
        }
        self.pending_save = None;
        self.save_task = None;
        self.phase = DocumentPhase::SaveFailed(error.into());
        true
    }

    pub fn conflict_save(&mut self, revision: u64, disk_hash: Option<String>) -> bool {
        if self.pending_save.as_ref().map(|save| save.revision) != Some(revision) {
            return false;
        }
        self.pending_save = None;
        self.save_task = None;
        self.autosave_task = None;
        self.phase = DocumentPhase::Conflict { disk_hash };
        true
    }

    pub fn mark_external(&mut self, disk_hash: Option<String>) {
        self.autosave_task = None;
        self.phase = DocumentPhase::ExternallyModified { disk_hash };
    }

    pub fn mark_deleted(&mut self) {
        self.autosave_task = None;
        self.save_task = None;
        self.reconcile_task = None;
        self.pending_save = None;
        self.pending_external_reload = None;
        self.phase = DocumentPhase::DeletedOnDisk;
    }

    pub fn restore_on_disk(&mut self, file: WorkspaceFileText) {
        let hash = file.content_hash.clone();
        self.encoding = writable_encoding(file.encoding);
        self.line_ending = writable_line_ending(file.line_ending);
        self.loaded_hash = hash.clone();
        self.saved_hash = hash;
        self.phase = read_only_reason(&file)
            .map(DocumentPhase::ReadOnly)
            .unwrap_or(DocumentPhase::Ready);
        self.file = Some(file);
    }

    pub fn queue_external_reload(&mut self, file: WorkspaceFileText) {
        self.pending_external_reload = Some(file);
        self.reconcile_task = None;
    }

    pub fn apply_external_reload(&mut self, file: WorkspaceFileText) {
        let hash = file.content_hash.clone();
        self.encoding = writable_encoding(file.encoding);
        self.line_ending = writable_line_ending(file.line_ending);
        self.lines = Arc::new(
            file.text
                .as_deref()
                .unwrap_or_default()
                .split('\n')
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
        );
        self.loaded_hash = hash.clone();
        self.saved_hash = hash;
        self.saved_revision = self.revision;
        self.phase = read_only_reason(&file)
            .map(DocumentPhase::ReadOnly)
            .unwrap_or(DocumentPhase::Ready);
        self.pending_external_reload = None;
        self.file = Some(file);
    }

    pub fn discard_changes(&mut self) {
        self.autosave_task = None;
        self.save_task = None;
        self.reconcile_task = None;
        self.pending_save = None;
        self.pending_external_reload = None;
        self.saved_revision = self.revision;
    }

    pub fn content_hash(&self) -> Option<&str> {
        self.file.as_ref()?.content_hash.as_deref()
    }

    pub fn estimated_retained_bytes(&self) -> usize {
        let file_text_bytes = self
            .file
            .as_ref()
            .and_then(|file| file.text.as_ref())
            .map_or(0, String::capacity);
        let pending_reload_bytes = self
            .pending_external_reload
            .as_ref()
            .and_then(|file| file.text.as_ref())
            .map_or(0, String::capacity);
        let line_bytes = self
            .lines
            .capacity()
            .saturating_mul(std::mem::size_of::<SharedString>())
            .saturating_add(
                self.lines
                    .iter()
                    .fold(0usize, |total, line| total.saturating_add(line.len())),
            );
        let pending_save_bytes = self
            .pending_save
            .as_ref()
            .map_or(0, |save| save.text.capacity());
        let editor_text_bytes = self
            .editor
            .is_some()
            .then_some(file_text_bytes)
            .unwrap_or(0);

        std::mem::size_of::<Self>()
            .saturating_add(file_text_bytes)
            .saturating_add(pending_reload_bytes)
            .saturating_add(line_bytes)
            .saturating_add(pending_save_bytes)
            .saturating_add(editor_text_bytes)
    }
}

fn read_only_reason(file: &WorkspaceFileText) -> Option<WorkspaceReadOnlyReason> {
    file.read_only_reason.or_else(|| {
        (file.truncated
            || file.text.is_none()
            || file.content_hash.is_none()
            || writable_encoding(file.encoding).is_none()
            || writable_line_ending(file.line_ending).is_none())
        .then_some(WorkspaceReadOnlyReason::NotRegularFile)
    })
}

fn writable_encoding(encoding: WorkspaceTextEncoding) -> Option<WorkspaceWritableEncoding> {
    match encoding {
        WorkspaceTextEncoding::Utf8 => Some(WorkspaceWritableEncoding::Utf8),
        WorkspaceTextEncoding::Utf8Bom => Some(WorkspaceWritableEncoding::Utf8Bom),
        WorkspaceTextEncoding::Binary | WorkspaceTextEncoding::Unsupported => None,
    }
}

fn writable_line_ending(
    line_ending: Option<WorkspaceLineEnding>,
) -> Option<WorkspaceWritableLineEnding> {
    match line_ending {
        Some(WorkspaceLineEnding::Lf | WorkspaceLineEnding::None) => {
            Some(WorkspaceWritableLineEnding::Lf)
        }
        Some(WorkspaceLineEnding::Crlf) => Some(WorkspaceWritableLineEnding::Crlf),
        Some(WorkspaceLineEnding::Mixed) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::WorkspaceTextEncoding;

    fn key(path: &str) -> DocumentKey {
        DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        }
    }

    fn text_file() -> WorkspaceFileText {
        WorkspaceFileText {
            checkout_id: "checkout-1".into(),
            path: "src/lib.rs".into(),
            text: Some("fn main() {}".into()),
            content_hash: Some("hash-1".into()),
            size: 12,
            modified_at: None,
            encoding: WorkspaceTextEncoding::Utf8,
            line_ending: Some(WorkspaceLineEnding::Lf),
            read_only_reason: None,
            truncated: false,
        }
    }

    #[test]
    fn programmatic_load_is_clean_and_user_edits_use_revisions() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        assert!(!document.is_dirty());
        assert!(document.is_editable());

        document.mark_user_edit();
        assert!(document.is_dirty());
        assert_eq!(document.revision, 1);
    }

    #[test]
    fn stale_key_or_generation_is_rejected() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        let generation = document.generation;
        assert!(document.accepts(&key("src/lib.rs"), generation));
        document.begin_load();
        assert!(!document.accepts(&key("src/lib.rs"), generation));
        assert!(!document.accepts(&key("src/main.rs"), document.generation));
    }

    #[test]
    fn unsupported_and_truncated_files_are_read_only() {
        let mut unsupported = text_file();
        unsupported.encoding = WorkspaceTextEncoding::Unsupported;
        unsupported.read_only_reason = Some(WorkspaceReadOnlyReason::UnsupportedEncoding);
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(unsupported);
        assert!(matches!(
            document.phase,
            DocumentPhase::ReadOnly(WorkspaceReadOnlyReason::UnsupportedEncoding)
        ));
        assert!(!document.is_editable());

        let mut truncated = text_file();
        truncated.truncated = true;
        document.set_loaded(truncated);
        assert!(matches!(document.phase, DocumentPhase::ReadOnly(_)));
    }

    #[test]
    fn save_snapshot_only_cleans_the_revision_it_captured() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        document.mark_user_edit();
        let pending = document.begin_save("fn first() {}".into()).unwrap();
        assert_eq!(pending.revision, 1);
        assert_eq!(pending.expected_content_hash, "hash-1");
        assert!(matches!(document.phase, DocumentPhase::Saving));

        document.mark_user_edit();
        assert!(document.finish_save(pending.revision, "hash-2".into()));
        assert!(document.is_dirty());
        assert_eq!(document.saved_revision, 1);
        assert_eq!(document.revision, 2);
        assert!(document.can_autosave());
    }

    #[test]
    fn save_uses_the_read_checkout_identity_not_the_chat_metadata() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        let mut file = text_file();
        file.checkout_id = "checkout-returned-by-engine".into();
        document.set_loaded(file);
        document.mark_user_edit();
        let pending = document.begin_save("preserved edit".into()).unwrap();
        assert_eq!(pending.expected_checkout_id, "checkout-returned-by-engine");
    }

    #[test]
    fn failed_and_conflicting_saves_preserve_dirty_content() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        document.mark_user_edit();
        let revision = document.begin_save("changed".into()).unwrap().revision;
        assert!(document.fail_save(revision, "offline"));
        assert!(document.is_dirty());
        assert!(matches!(document.phase, DocumentPhase::SaveFailed(_)));
        assert!(document.can_save());
        assert!(!document.can_autosave());

        let retry_revision = document.begin_save("retry".into()).unwrap().revision;
        assert!(matches!(document.phase, DocumentPhase::Saving));
        assert!(document.fail_save(retry_revision, "still offline"));
        assert!(!document.can_autosave());

        document.mark_user_edit();
        assert!(matches!(document.phase, DocumentPhase::Ready));
        assert!(document.can_autosave());
        let revision = document
            .begin_save("changed again".into())
            .unwrap()
            .revision;
        assert!(document.conflict_save(revision, Some("disk-hash".into())));
        assert!(document.is_dirty());
        assert!(matches!(
            document.phase,
            DocumentPhase::Conflict {
                disk_hash: Some(ref hash)
            } if hash == "disk-hash"
        ));
        assert!(!document.can_autosave());
    }

    #[test]
    fn stale_save_result_cannot_clean_a_new_request() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        document.mark_user_edit();
        let revision = document.begin_save("changed".into()).unwrap().revision;
        assert!(!document.finish_save(revision.wrapping_add(1), "wrong-hash".into()));
        assert!(document.is_dirty());
        assert!(matches!(document.phase, DocumentPhase::Saving));
    }

    #[test]
    fn external_reload_is_clean_but_external_dirty_state_blocks_autosave() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        let mut reloaded = text_file();
        reloaded.text = Some("fn external() {}".into());
        reloaded.content_hash = Some("hash-2".into());
        document.apply_external_reload(reloaded);
        assert_eq!(document.saved_hash.as_deref(), Some("hash-2"));
        assert!(!document.is_dirty());

        document.mark_user_edit();
        document.mark_external(Some("hash-3".into()));
        document.mark_user_edit();
        assert_eq!(document.revision, 2);
        assert!(!document.can_autosave());
        assert!(matches!(
            document.phase,
            DocumentPhase::ExternallyModified {
                disk_hash: Some(ref hash)
            } if hash == "hash-3"
        ));
    }

    #[test]
    fn deletion_preserves_dirty_revision_and_stops_autosave() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        document.mark_user_edit();
        document.mark_deleted();
        assert!(document.is_dirty());
        assert!(document.is_editable());
        assert!(!document.can_autosave());
        assert!(matches!(document.phase, DocumentPhase::DeletedOnDisk));
    }

    #[test]
    fn same_content_recreation_restores_disk_state_without_losing_dirty_edits() {
        let mut clean = FileDocument::loading(key("src/lib.rs"));
        clean.set_loaded(text_file());
        clean.mark_deleted();
        clean.restore_on_disk(text_file());
        assert!(matches!(clean.phase, DocumentPhase::Ready));
        assert!(!clean.is_dirty());

        let mut dirty = FileDocument::loading(key("src/lib.rs"));
        dirty.set_loaded(text_file());
        dirty.mark_user_edit();
        dirty.mark_deleted();
        dirty.restore_on_disk(text_file());
        assert!(matches!(dirty.phase, DocumentPhase::Ready));
        assert!(dirty.is_dirty());
        assert!(dirty.can_autosave());
    }

    #[test]
    fn explicit_discard_resolves_dirty_state_for_lifecycle_exit() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        document.mark_user_edit();
        assert!(document.is_dirty());
        document.discard_changes();
        assert!(!document.is_dirty());
        assert!(document.pending_save.is_none());
    }
}
