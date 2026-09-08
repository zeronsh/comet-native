use std::{collections::HashSet, time::Duration};

use gpui::Context;
use zeron_proto::{WorkspaceFileChangeKind, WorkspaceFileChanges};

use super::{FilesEvent, FilesSurface, client::WorkspaceFilesClient, model::parent_path};

impl FilesSurface {
    pub(super) fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if self.watch_task.is_some() {
            return;
        }
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let client = WorkspaceFilesClient::new(engine, context);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            loop {
                match client.watch().await {
                    Ok(mut receiver) => {
                        let _ = this.update(cx, |surface, cx| {
                            surface.watch_error = None;
                            cx.notify();
                        });
                        while let Some(value) = receiver.recv().await {
                            let frame = serde_json::from_value::<WorkspaceFileChanges>(value);
                            if this
                                .update(cx, |surface, cx| match frame {
                                    Ok(frame) => surface.apply_workspace_changes(frame, cx),
                                    Err(error) => {
                                        surface.watch_error = Some(
                                            format!("File updates could not be decoded: {error}")
                                                .into(),
                                        );
                                        cx.notify();
                                    }
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        if this
                            .update(cx, |surface, cx| {
                                surface.watch_error =
                                    Some("File updates interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        if this
                            .update(cx, |surface, cx| {
                                surface.watch_error = Some(error.to_string().into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        }));
    }

    fn apply_workspace_changes(&mut self, frame: WorkspaceFileChanges, cx: &mut Context<Self>) {
        let gap = sequence_needs_resync(self.watch_sequence, frame.sequence);
        tracing::trace!(
            sequence = frame.sequence,
            previous_sequence = ?self.watch_sequence,
            resync_required = frame.resync_required,
            gap,
            change_count = frame.changes.len(),
            "workspace file changes received by UI"
        );
        self.watch_sequence = Some(frame.sequence);
        if frame.resync_required || gap {
            self.refresh(cx);
            self.reconcile_open_documents(cx);
            return;
        }

        let mut parents = HashSet::new();
        for change in frame.changes {
            match change.kind {
                WorkspaceFileChangeKind::Created => {
                    self.reconcile_created_documents(&change.path, cx);
                    if let Some(parent) = parent_path(&change.path) {
                        parents.insert(parent);
                    }
                }
                WorkspaceFileChangeKind::Modified => {
                    self.reconcile_document(change.path, cx);
                }
                WorkspaceFileChangeKind::Removed => {
                    self.tree.remove(&change.path);
                    self.mark_document_deleted(&change.path, cx);
                    if let Some(parent) = parent_path(&change.path) {
                        parents.insert(parent);
                    }
                }
                WorkspaceFileChangeKind::Renamed => {
                    if let Some(old_path) = change.old_path {
                        self.tree.remove(&old_path);
                        for (old_path, new_path) in
                            self.rename_documents(&old_path, change.path.clone(), cx)
                        {
                            cx.emit(FilesEvent::FileRenamed { old_path, new_path });
                        }
                        if let Some(parent) = parent_path(&old_path) {
                            parents.insert(parent);
                        }
                    }
                    // Atomic replacement tools can report a temporary file being
                    // renamed over an already-open destination document.
                    self.reconcile_created_documents(&change.path, cx);
                    if let Some(parent) = parent_path(&change.path) {
                        parents.insert(parent);
                    }
                }
            }
        }

        let reload = parents
            .into_iter()
            .filter(|parent| self.tree.is_expanded(parent) && self.tree.is_directory_loaded(parent))
            .collect::<Vec<_>>();
        for parent in &reload {
            self.tree.invalidate_directory(parent);
        }
        self.sync_tree_list();
        for parent in reload {
            self.load_directory(parent, None, cx);
        }
        cx.notify();
    }
}

pub(super) fn sequence_needs_resync(previous: Option<u64>, next: u64) -> bool {
    previous.is_some_and(|previous| next != previous.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_gaps_require_resync() {
        assert!(!sequence_needs_resync(None, 40));
        assert!(!sequence_needs_resync(Some(40), 41));
        assert!(sequence_needs_resync(Some(40), 42));
        assert!(sequence_needs_resync(Some(40), 40));
    }
}
