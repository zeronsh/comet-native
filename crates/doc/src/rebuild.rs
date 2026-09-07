//! M1 epoch rebuild (docs/chat2-sync.md) — the whale-healing step.
//!
//! On first chat2 seed the host does NOT upload its fat doc: it rebuilds a
//! thin one. Every entry is copied with the A1 strip applied retroactively —
//! fat inline tool outputs become one-line summaries, inline diffs become
//! per-file stats — and the stripped full payloads are returned so the host
//! can upload them to the A2 sidecar (existing whale outputs stay viewable;
//! zero information loss). Commands: only unresolved entries carry over (the
//! `processed_commands` mark-before-execute ledger protects against
//! re-execution regardless).
//!
//! `meta.epoch = 2` marks the new lineage. Other devices seeing a
//! `roomGen: 2` chat with a local `epoch < 2` doc discard-and-adopt (M3);
//! the epoch check makes re-entry a no-op.

use crate::SessionCommandStatus;
use crate::parts::{MessagePart, SidecarPayload, diff_stat, summarize_tool_output};
use crate::schema::{DocError, SessionDoc};

/// Doc lineage epoch written by [`rebuild_thin_doc`]. Pre-migration s2 docs
/// read back as 0.
pub const THIN_DOC_EPOCH: u32 = 2;

/// Read `meta.epoch` (0 = pre-migration doc).
pub fn doc_epoch(doc: &SessionDoc) -> u32 {
    match doc.doc().get_map("meta").get("epoch") {
        Some(loro::ValueOrContainer::Value(loro::LoroValue::I64(n))) => n.max(0) as u32,
        _ => 0,
    }
}

/// A completed rebuild: the thin doc plus everything it stripped out (owed
/// to the sidecar) and the copy accounting.
pub struct ThinRebuild {
    pub doc: SessionDoc,
    /// Full outputs/diffs stripped from old fat parts — upload to the A2
    /// sidecar under `{chatId}/{partId}[.diff]` before flipping `roomGen`.
    pub sidecar: Vec<SidecarPayload>,
    pub entries: usize,
    pub commands_copied: usize,
}

/// Rebuild a thin epoch-2 doc from a (possibly fat) source doc.
///
/// Pure over doc contents — no I/O; idempotent in the sense that rebuilding
/// an already-thin doc strips nothing and owes the sidecar nothing.
pub fn rebuild_thin_doc(source: &SessionDoc) -> Result<ThinRebuild, DocError> {
    let chat_id = source
        .chat_id()
        .ok_or_else(|| DocError::Schema("source doc has no chatId".into()))?;
    let thin = SessionDoc::init(&chat_id)?;
    thin.doc()
        .get_map("meta")
        .insert("epoch", THIN_DOC_EPOCH as i64)?;

    if let Some(usage) = source.context_usage() {
        thin.update_context_usage(usage.tokens, usage.window)?;
    }
    let mut sidecar = Vec::new();
    let entries = source.read_entries()?;
    let entry_count = entries.len();
    for mut entry in entries {
        for part in entry.parts.iter_mut() {
            if let Some(payload) = strip_part(&chat_id, part) {
                sidecar.push(payload);
            }
        }
        thin.push_message(&entry)?;
    }

    let mut commands_copied = 0;
    for command in source.read_commands()? {
        // Only unresolved work crosses the epoch: resolved/expired ledger
        // history is exactly the bulk a wedged room can't afford to carry.
        if command.status == SessionCommandStatus::Pending {
            thin.queue_command(&command)?;
            commands_copied += 1;
        }
    }
    thin.doc().commit();

    Ok(ThinRebuild {
        doc: thin,
        sidecar,
        entries: entry_count,
        commands_copied,
    })
}

/// Apply the A1 strip to one part in place. `Some` = payload owed to the
/// sidecar (the part was fat). Already-thin parts pass through untouched.
fn strip_part(chat_id: &str, part: &mut MessagePart) -> Option<SidecarPayload> {
    let MessagePart::Tool {
        id,
        output,
        diff,
        output_ref,
        output_bytes,
        diff_ref,
        diff_stats,
        ..
    } = part
    else {
        return None;
    };
    let mut payload = SidecarPayload {
        part_id: id.clone(),
        output: None,
        diff: None,
    };
    // Fat inline output = text with no sidecar ref (the new fold always
    // stamps `output_ref` beside a summary). Blank output just drops.
    if output_ref.is_none()
        && let Some(full) = output.take()
    {
        if full.trim().is_empty() {
            *output = None;
        } else {
            *output = summarize_tool_output(&full);
            *output_bytes = Some(full.len() as u64);
            *output_ref = Some(format!("{chat_id}/{id}"));
            payload.output = Some(full);
        }
    }
    // Inline diff text dies entirely: stats + ref replace it.
    if let Some(full) = diff.take() {
        *diff_stats = Some(vec![diff_stat(&full)]);
        if diff_ref.is_none() {
            *diff_ref = Some(format!("{chat_id}/{id}.diff"));
        }
        payload.diff = Some(full);
    }
    (payload.output.is_some() || payload.diff.is_some()).then_some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{MessageRole, SessionMessageEntry};
    use crate::{MessageStatus, SessionCommandEntry, SessionCommandPayload};
    use zeron_proto::{ToolCall, ToolDiff};

    /// ~4KB of varied output — repeated text would compress away inside the
    /// Loro snapshot and hide the size win the assertion checks.
    fn fat_output() -> String {
        let mut out = String::new();
        let mut x: u64 = 0x9e37_79b9;
        for i in 0..200 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push_str(&format!("line {i}: {x:016x} {:08x}\n", x >> 13));
        }
        out
    }

    fn fat_entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![
                MessagePart::Text {
                    id: format!("{id}-t"),
                    text: "did the thing".into(),
                },
                MessagePart::Tool {
                    id: format!("{id}-tool"),
                    call: ToolCall::Exec {
                        command: "cargo test".into(),
                    },
                    is_error: false,
                    resolved: true,
                    output: Some(fat_output()), // ~4KB fat inline, incompressible
                    diff: Some(ToolDiff {
                        path: "/w/a.rs".into(),
                        old_text: Some("a\nb\n".into()),
                        new_text: "a\nc\nd\n".into(),
                    }),
                    output_ref: None,
                    output_bytes: None,
                    diff_ref: None,
                    diff_stats: None,
                    subagent_ref: None,
                    subagent_status: None,
                    subagent_tail: None,
                },
            ],
            created_at: 5,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn command(id: &str, status: SessionCommandStatus) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload: SessionCommandPayload::Interrupt {},
            issued_by: "dev-b".into(),
            issued_at: 9,
            based_on: None,
            expires_at: None,
            status,
            resolution: None,
        }
    }

    #[test]
    fn rebuild_strips_fat_parts_and_owes_them_to_the_sidecar() {
        let source = SessionDoc::init("chat-w").unwrap();
        source.push_message(&fat_entry("m1")).unwrap();
        source
            .queue_command(&command("c1", SessionCommandStatus::Pending))
            .unwrap();
        source
            .queue_command(&command("c2", SessionCommandStatus::Applied))
            .unwrap();
        let fat_size = source.export_snapshot().unwrap().len();

        let rebuilt = rebuild_thin_doc(&source).unwrap();
        assert_eq!(doc_epoch(&rebuilt.doc), THIN_DOC_EPOCH);
        assert_eq!(doc_epoch(&source), 0);
        assert_eq!((rebuilt.entries, rebuilt.commands_copied), (1, 1));

        // The whale math: the thin doc must be dramatically smaller.
        let thin_size = rebuilt.doc.export_snapshot().unwrap().len();
        assert!(
            thin_size * 2 < fat_size,
            "thin {thin_size} vs fat {fat_size}"
        );

        let entries = rebuilt.doc.read_entries().unwrap();
        match &entries[0].parts[1] {
            MessagePart::Tool {
                output,
                output_ref,
                output_bytes,
                diff,
                diff_ref,
                diff_stats,
                ..
            } => {
                assert_eq!(
                    output.as_deref(),
                    fat_output()
                        .lines()
                        .next()
                        .map(|l| format!("{l}…"))
                        .as_deref()
                );
                assert_eq!(output_ref.as_deref(), Some("chat-w/m1-tool"));
                assert_eq!(*output_bytes, Some(fat_output().len() as u64));
                assert!(diff.is_none());
                assert_eq!(diff_ref.as_deref(), Some("chat-w/m1-tool.diff"));
                let stats = diff_stats.as_ref().unwrap();
                assert_eq!((stats[0].additions, stats[0].deletions), (2, 1));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Everything stripped is owed to the sidecar — nothing is lost.
        assert_eq!(rebuilt.sidecar.len(), 1);
        assert_eq!(rebuilt.sidecar[0].part_id, "m1-tool");
        assert!(rebuilt.sidecar[0].output.as_deref().unwrap().len() > 3000);
        assert_eq!(rebuilt.sidecar[0].diff.as_ref().unwrap().path, "/w/a.rs");

        // Only the unresolved command crossed the epoch.
        let commands = rebuilt.doc.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].id, "c1");
    }

    #[test]
    fn rebuilding_a_thin_doc_is_a_no_op_strip() {
        let source = SessionDoc::init("chat-w").unwrap();
        source.push_message(&fat_entry("m1")).unwrap();
        let first = rebuild_thin_doc(&source).unwrap();
        let second = rebuild_thin_doc(&first.doc).unwrap();
        assert!(
            second.sidecar.is_empty(),
            "already-thin parts owe the sidecar nothing"
        );
        assert_eq!(
            first.doc.read_entries().unwrap(),
            second.doc.read_entries().unwrap()
        );
    }
}
