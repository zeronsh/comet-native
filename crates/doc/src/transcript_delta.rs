//! Delta frames for `WatchDocMessages`.
//!
//! The watch used to re-serialize the entire transcript on every 120ms commit
//! tick (measured at 1.13MB per frame on a 1.6MB chat, ~4 copies deep through
//! the RPC hop). A frame is now either a full `reset` (first frame, and the
//! fallback when a diff would approach transcript size) or the changed
//! entries only — during streaming that is one entry per tick.
//!
//! Both viewports share this module (the `zeron_proto::view` rule: derivations
//! that must not diverge per surface live in one place).

use serde::{Deserialize, Serialize};

use crate::parts::MessagePart;
use crate::schema::SessionMessageEntry;

/// Transcript changes and the current host-owned context snapshot travel together.
/// Older readers ignore `contextUsage`; older hosts decode as unknown usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptUpdate {
    #[serde(flatten)]
    pub frame: TranscriptFrame,
    #[serde(default)]
    pub context_usage: Option<zeron_proto::ContextUsage>,
}

/// One `WatchDocMessages` stream item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptFrame {
    Reset {
        reset: Vec<SessionMessageEntry>,
    },
    Delta {
        #[serde(default)]
        upsert: Vec<TranscriptUpsert>,
        #[serde(default)]
        append: Vec<TextAppend>,
        #[serde(default)]
        remove: Vec<String>,
        /// Expected transcript length after applying this frame — the desync
        /// tripwire: a consumer that lands elsewhere resubscribes for a reset.
        count: usize,
    },
}

/// An inserted or replaced entry, positioned after `after` (`None` = head).
/// Anchors are prior upserts of the same frame or unchanged entries, so
/// applying upserts in frame order always finds them settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptUpsert {
    pub after: Option<String>,
    pub entry: SessionMessageEntry,
}

/// A pure text-tail append to one part — the streaming hot path. Entry-level
/// upserts re-send the whole live entry per tick, which for a long single
/// reply is the whole reply again (continuations re-join before the watch);
/// this carries only the new tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextAppend {
    pub entry: String,
    pub part: String,
    pub text: String,
    /// Total text length of the part after the append (desync tripwire).
    pub len: usize,
}

impl TranscriptFrame {
    pub fn reset(entries: &[SessionMessageEntry]) -> Self {
        Self::Reset {
            reset: entries.to_vec(),
        }
    }

    pub fn is_empty_delta(&self) -> bool {
        matches!(
            self,
            Self::Delta { upsert, append, remove, .. }
                if upsert.is_empty() && append.is_empty() && remove.is_empty()
        )
    }
}

/// When `next` is `prev` plus text appended to exactly one text-bodied part
/// (text or reasoning; same position, all other fields and parts identical),
/// the change is a [`TextAppend`]. Any other difference falls back to a full
/// upsert.
fn try_text_append(prev: &SessionMessageEntry, next: &SessionMessageEntry) -> Option<TextAppend> {
    if prev.id != next.id
        || prev.role != next.role
        || prev.created_at != next.created_at
        || prev.device_id != next.device_id
        || prev.status != next.status
        || prev.continuation_of != next.continuation_of
        || prev.parts.len() != next.parts.len()
    {
        return None;
    }
    let mut append = None;
    for (p, n) in prev.parts.iter().zip(&next.parts) {
        if p == n {
            continue;
        }
        let ((pid, pt), (nid, nt)) = match (p, n) {
            (MessagePart::Text { id: pid, text: pt }, MessagePart::Text { id: nid, text: nt })
            | (
                MessagePart::Reasoning { id: pid, text: pt },
                MessagePart::Reasoning { id: nid, text: nt },
            ) => ((pid, pt), (nid, nt)),
            _ => return None,
        };
        if pid != nid || !nt.starts_with(pt.as_str()) || append.is_some() {
            return None;
        }
        append = Some(TextAppend {
            entry: next.id.clone(),
            part: nid.clone(),
            text: nt[pt.len()..].to_string(),
            len: nt.len(),
        });
    }
    append
}

/// Diff two transcript states into a frame. An entry is upserted when it is
/// new, its content changed, or its predecessor changed (a Loro list merge can
/// interleave a remote entry mid-list). Falls back to `Reset` when the delta
/// would carry most of the transcript anyway.
pub fn diff_transcript(
    prev: &[SessionMessageEntry],
    next: &[SessionMessageEntry],
) -> TranscriptFrame {
    let prev_by_id: std::collections::HashMap<&str, (usize, &SessionMessageEntry)> = prev
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), (i, e)))
        .collect();
    let next_ids: std::collections::HashSet<&str> = next.iter().map(|e| e.id.as_str()).collect();

    let remove: Vec<String> = prev
        .iter()
        .filter(|e| !next_ids.contains(e.id.as_str()))
        .map(|e| e.id.clone())
        .collect();

    let mut upsert = Vec::new();
    let mut append = Vec::new();
    for (i, entry) in next.iter().enumerate() {
        let after = i.checked_sub(1).map(|p| next[p].id.as_str());
        match prev_by_id.get(entry.id.as_str()) {
            Some((prev_ix, prev_entry)) => {
                let prev_after = prev_ix.checked_sub(1).map(|p| prev[p].id.as_str());
                if *prev_entry == entry && prev_after == after {
                    continue;
                }
                if prev_after == after
                    && let Some(text_append) = try_text_append(prev_entry, entry)
                {
                    append.push(text_append);
                    continue;
                }
                upsert.push(TranscriptUpsert {
                    after: after.map(str::to_string),
                    entry: entry.clone(),
                });
            }
            None => upsert.push(TranscriptUpsert {
                after: after.map(str::to_string),
                entry: entry.clone(),
            }),
        }
    }

    // A delta touching most rows serializes like a reset but applies slower.
    if upsert.len() * 2 >= next.len().max(1) && next.len() > 4 {
        return TranscriptFrame::reset(next);
    }
    TranscriptFrame::Delta {
        upsert,
        append,
        remove,
        count: next.len(),
    }
}

/// A frame that could not be applied cleanly — the consumer's copy has
/// diverged (skew, missed frame) and it should resubscribe for a reset.
#[derive(Debug, thiserror::Error)]
#[error("transcript delta desync: {0}")]
pub struct TranscriptDesync(pub String);

/// Apply a frame in place. On any error the state is unreliable — resubscribe.
pub fn apply_transcript_frame(
    current: &mut Vec<SessionMessageEntry>,
    frame: TranscriptFrame,
) -> Result<(), TranscriptDesync> {
    match frame {
        TranscriptFrame::Reset { reset } => {
            *current = reset;
            Ok(())
        }
        TranscriptFrame::Delta {
            upsert,
            append,
            remove,
            count,
        } => {
            if !remove.is_empty() {
                let gone: std::collections::HashSet<&str> =
                    remove.iter().map(String::as_str).collect();
                current.retain(|e| !gone.contains(e.id.as_str()));
            }
            for TranscriptUpsert { after, entry } in upsert {
                if let Some(existing) = current.iter().position(|e| e.id == entry.id) {
                    current.remove(existing);
                }
                let at = match &after {
                    None => 0,
                    Some(anchor) => match current.iter().position(|e| &e.id == anchor) {
                        Some(ix) => ix + 1,
                        None => {
                            return Err(TranscriptDesync(format!("missing anchor {anchor}")));
                        }
                    },
                };
                current.insert(at, entry);
            }
            for TextAppend {
                entry,
                part,
                text,
                len,
            } in append
            {
                let Some(target) = current.iter_mut().find(|e| e.id == entry) else {
                    return Err(TranscriptDesync(format!("missing append entry {entry}")));
                };
                let tail = target.parts.iter_mut().find_map(|p| match p {
                    MessagePart::Text { id, text } | MessagePart::Reasoning { id, text }
                        if *id == part =>
                    {
                        Some(text)
                    }
                    _ => None,
                });
                let Some(tail) = tail else {
                    return Err(TranscriptDesync(format!("missing append part {part}")));
                };
                tail.push_str(&text);
                if tail.len() != len {
                    return Err(TranscriptDesync(format!(
                        "append length mismatch on {entry}#{part}: have {}, expected {len}",
                        tail.len()
                    )));
                }
            }
            if current.len() != count {
                return Err(TranscriptDesync(format!(
                    "count mismatch: have {}, expected {count}",
                    current.len()
                )));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::MessagePart;
    use crate::schema::MessageRole;

    fn entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn apply(prev: &[SessionMessageEntry], next: &[SessionMessageEntry]) {
        let frame = diff_transcript(prev, next);
        // Round-trip through JSON: the wire shape must survive serde.
        let json = serde_json::to_value(&frame).unwrap();
        let frame: TranscriptFrame = serde_json::from_value(json).unwrap();
        let mut current = prev.to_vec();
        apply_transcript_frame(&mut current, frame).unwrap();
        assert_eq!(&current, next);
    }

    #[test]
    fn append_and_edit_round_trip() {
        let a = entry("a", "hello");
        let b0 = entry("b", "wor");
        let b1 = entry("b", "world");
        apply(&[], &[a.clone()]);
        apply(&[a.clone()], &[a.clone(), b0.clone()]);
        apply(&[a.clone(), b0], &[a, b1]);
    }

    #[test]
    fn remove_and_mid_insert_round_trip() {
        let a = entry("a", "1");
        let b = entry("b", "2");
        let c = entry("c", "3");
        apply(&[a.clone(), b.clone(), c.clone()], &[a.clone(), c.clone()]);
        // Remote merge lands b between a and c.
        apply(&[a.clone(), c.clone()], &[a, b, c]);
    }

    #[test]
    fn streaming_tick_is_a_text_append() {
        let a = entry("a", "prompt");
        let b0 = entry("b", "streaming…");
        let b1 = entry("b", "streaming… more");
        let frame = diff_transcript(&[a.clone(), b0.clone()], &[a.clone(), b1.clone()]);
        match &frame {
            TranscriptFrame::Delta {
                upsert,
                append,
                remove,
                ..
            } => {
                // The hot path: only the new tokens travel, never the entry.
                assert!(upsert.is_empty());
                assert_eq!(append.len(), 1);
                assert_eq!(append[0].text, " more");
                assert!(remove.is_empty());
            }
            other => panic!("expected delta, got {other:?}"),
        }
        apply(&[a.clone(), b0], &[a, b1]);
    }

    #[test]
    fn streaming_reasoning_tick_is_a_text_append() {
        let mut b0 = entry("b", "prompt");
        b0.parts = vec![MessagePart::Reasoning {
            id: "r0".into(),
            text: "thinking".into(),
        }];
        let mut b1 = b0.clone();
        b1.parts = vec![MessagePart::Reasoning {
            id: "r0".into(),
            text: "thinking more".into(),
        }];
        let frame = diff_transcript(std::slice::from_ref(&b0), std::slice::from_ref(&b1));
        match &frame {
            TranscriptFrame::Delta { upsert, append, .. } => {
                assert!(upsert.is_empty(), "reasoning growth must ride the hot path");
                assert_eq!(append.len(), 1);
                assert_eq!(append[0].text, " more");
            }
            other => panic!("expected delta, got {other:?}"),
        }
        apply(&[b0], &[b1]);
    }

    #[test]
    fn non_append_change_falls_back_to_upsert() {
        // Same id but a rewritten (non-prefix) text must re-send the entry.
        let b0 = entry("b", "draft text");
        let b1 = entry("b", "final");
        let frame = diff_transcript(&[b0.clone()], &[b1.clone()]);
        match &frame {
            TranscriptFrame::Delta { upsert, append, .. } => {
                assert_eq!(upsert.len(), 1);
                assert!(append.is_empty());
            }
            other => panic!("expected delta, got {other:?}"),
        }
        apply(&[b0], &[b1]);
    }

    #[test]
    fn append_length_mismatch_is_desync() {
        let frame = TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![TextAppend {
                entry: "a".into(),
                part: "t0".into(),
                text: "x".into(),
                len: 99,
            }],
            remove: vec![],
            count: 1,
        };
        let mut current = vec![entry("a", "hello")];
        assert!(apply_transcript_frame(&mut current, frame).is_err());
    }

    #[test]
    fn large_change_falls_back_to_reset() {
        let prev: Vec<_> = (0..10).map(|i| entry(&format!("p{i}"), "x")).collect();
        let next: Vec<_> = (0..10).map(|i| entry(&format!("n{i}"), "y")).collect();
        assert!(matches!(
            diff_transcript(&prev, &next),
            TranscriptFrame::Reset { .. }
        ));
    }

    #[test]
    fn desync_surfaces_instead_of_corrupting() {
        let frame = TranscriptFrame::Delta {
            upsert: vec![TranscriptUpsert {
                after: Some("missing".into()),
                entry: entry("x", "1"),
            }],
            append: vec![],
            remove: vec![],
            count: 2,
        };
        let mut current = Vec::new();
        assert!(apply_transcript_frame(&mut current, frame).is_err());
    }
}

#[cfg(test)]
mod context_update_tests {
    use super::*;
    #[test]
    fn usage_envelope_is_compatible_with_old_hosts_and_readers() {
        let old = serde_json::json!({"reset": []});
        let update: TranscriptUpdate = serde_json::from_value(old).unwrap();
        assert_eq!(update.context_usage, None);
        let value = serde_json::to_value(TranscriptUpdate {
            frame: TranscriptFrame::reset(&[]),
            context_usage: Some(zeron_proto::ContextUsage {
                tokens: Some(0),
                window: Some(200000),
            }),
        })
        .unwrap();
        assert_eq!(value["contextUsage"]["tokens"], 0);
        assert!(matches!(
            serde_json::from_value::<TranscriptFrame>(value).unwrap(),
            TranscriptFrame::Reset { .. }
        ));
    }
}
