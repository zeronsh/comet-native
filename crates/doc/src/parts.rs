//! Message parts: the event fold, the render-only privacy policy, and continuation splitting.
//!
//! Ports of `packages/control/src/parts.ts` (fold) and
//! `packages/session-doc/src/{render-parts,messages}.ts`.

use serde::{Deserialize, Serialize};

use comet_proto::{AgentEvent, ToolCall, UserInputQuestion};

use crate::constants::MSG_INLINE_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    Streaming,
    Complete,
    Aborted,
}

/// One rendered part of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessagePart {
    Text {
        id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        call: ToolCall,
        #[serde(default)]
        is_error: bool,
        /// True once a ToolResult arrived.
        #[serde(default)]
        resolved: bool,
    },
    #[serde(rename_all = "camelCase")]
    Input {
        id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default)]
        resolved: bool,
    },
    Error {
        id: String,
        message: String,
    },
}

impl MessagePart {
    pub fn id(&self) -> &str {
        match self {
            MessagePart::Text { id, .. }
            | MessagePart::Tool { id, .. }
            | MessagePart::Input { id, .. }
            | MessagePart::Error { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } => text.len(),
            MessagePart::Tool { call, .. } => serde_json::to_vec(call).map_or(0, |v| v.len()),
            MessagePart::Input { questions, .. } => {
                serde_json::to_vec(questions).map_or(0, |v| v.len())
            }
            MessagePart::Error { message, .. } => message.len(),
        }
    }
}

/// Immutably fold one agent event into a parts accumulator.
///
/// Semantics from comet `foldEventIntoParts`:
/// - `SessionStarted` / `Steered` reset the accumulator (turn boundary — makes replay safe).
/// - `TextDelta` appends to the trailing text part, or starts a new one if the trail is not text
///   (a tool call in between breaks the text block).
/// - `ToolCall` appends, or refreshes in place when the id already exists (SDK retry idempotence).
/// - `ToolResult` marks the matching tool part resolved / errored in place.
/// - `InputRequested` appends an input part; `InputResolved` marks it resolved.
/// - `Error` and `Done{error}` become visible error parts.
pub fn fold_event_into_parts(parts: &[MessagePart], event: &AgentEvent) -> Vec<MessagePart> {
    let mut out: Vec<MessagePart> = parts.to_vec();
    match event {
        AgentEvent::SessionStarted { .. } | AgentEvent::Steered { .. } => {
            return Vec::new();
        }
        AgentEvent::TextDelta { text } => {
            if let Some(MessagePart::Text { text: tail, .. }) = out.last_mut() {
                tail.push_str(text);
            } else {
                let id = format!("t{}", out.len());
                out.push(MessagePart::Text {
                    id,
                    text: text.clone(),
                });
            }
        }
        AgentEvent::ReasoningDelta { .. } => {
            // Reasoning is not rendered as a transcript part (matches comet).
        }
        AgentEvent::ToolCall { id, call } => {
            if let Some(existing) = out.iter_mut().find_map(|p| match p {
                MessagePart::Tool {
                    id: pid, call: c, ..
                } if pid == id => Some(c),
                _ => None,
            }) {
                *existing = call.clone();
            } else {
                out.push(MessagePart::Tool {
                    id: id.clone(),
                    call: call.clone(),
                    is_error: false,
                    resolved: false,
                });
            }
        }
        AgentEvent::ToolResult { id, is_error } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
                }
            }
        }
        AgentEvent::InputRequested {
            request_id,
            questions,
        } => {
            let id = format!("in-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Input {
                    id,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: false,
                });
            }
        }
        AgentEvent::InputResolved { request_id } => {
            for p in out.iter_mut() {
                if let MessagePart::Input {
                    request_id: rid,
                    resolved,
                    ..
                } = p
                    && rid == request_id
                {
                    *resolved = true;
                }
            }
        }
        AgentEvent::Error { message } => {
            let id = format!("e{}", out.len());
            out.push(MessagePart::Error {
                id,
                message: message.clone(),
            });
        }
        AgentEvent::Done { error, .. } => {
            if let Some(message) = error {
                let id = format!("e{}", out.len());
                out.push(MessagePart::Error {
                    id,
                    message: message.clone(),
                });
            }
        }
        AgentEvent::AssistantMessageCompleted { .. } | AgentEvent::Usage { .. } => {}
    }
    out
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path / pattern / url / query / todo items / server+tool names.
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, Mcp/Unknown input.
/// Full inputs remain only in the host's local run journal. Idempotent.
pub fn sanitize_tool_call(call: &ToolCall) -> ToolCall {
    match call {
        ToolCall::WriteFile { path, .. } => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        ToolCall::EditFile { path, .. } => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        ToolCall::WebFetch { url, .. } => ToolCall::WebFetch {
            url: url.clone(),
            prompt: None,
        },
        ToolCall::Mcp { server, tool, .. } => ToolCall::Mcp {
            server: server.clone(),
            tool: tool.clone(),
            input: None,
        },
        ToolCall::Unknown { name, .. } => ToolCall::Unknown {
            name: name.clone(),
            input: None,
        },
        other => other.clone(),
    }
}

/// Deterministic continuation id: `"{root}#c{n}"`.
pub fn continuation_id(root: &str, index: usize) -> String {
    format!("{root}#c{index}")
}

/// Split an oversized parts list into chunks each under `MSG_INLINE_MAX` bytes.
///
/// Splitting happens at part boundaries; an oversized text part is itself chunked at char
/// boundaries. Returns one Vec per resulting entry — the first keeps the root id, the rest are
/// continuations (`continuation_id(root, i)`), matching `splitMessageEntry` in comet.
pub fn split_parts(parts: &[MessagePart]) -> Vec<Vec<MessagePart>> {
    let mut chunks: Vec<Vec<MessagePart>> = vec![Vec::new()];
    let mut current_bytes = 0usize;

    let push_part = |chunks: &mut Vec<Vec<MessagePart>>, current: &mut usize, part: MessagePart| {
        let len = part.byte_len();
        if *current > 0 && *current + len > MSG_INLINE_MAX {
            chunks.push(Vec::new());
            *current = 0;
        }
        *current += len;
        chunks.last_mut().unwrap().push(part);
    };

    for part in parts {
        match part {
            MessagePart::Text { id, text } if text.len() > MSG_INLINE_MAX => {
                // Chunk oversized text at char boundaries.
                let mut start = 0usize;
                let mut piece = 0usize;
                while start < text.len() {
                    let mut end = (start + MSG_INLINE_MAX).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // Guard: ensure forward progress on pathological boundaries.
                    if end <= start {
                        end = text.len();
                    }
                    let sub = MessagePart::Text {
                        id: if piece == 0 {
                            id.clone()
                        } else {
                            format!("{id}~{piece}")
                        },
                        text: text[start..end].to_string(),
                    };
                    push_part(&mut chunks, &mut current_bytes, sub);
                    start = end;
                    piece += 1;
                }
            }
            other => push_part(&mut chunks, &mut current_bytes, other.clone()),
        }
    }
    chunks
}

/// Render-time inverse of splitting: concatenate continuation entries' parts in list order.
pub fn join_continuations(entries: Vec<Vec<MessagePart>>) -> Vec<MessagePart> {
    entries.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn text_deltas_merge_until_broken_by_tool() {
        let mut parts = Vec::new();
        parts = fold_event_into_parts(&parts, &text_delta("Hello "));
        parts = fold_event_into_parts(&parts, &text_delta("world"));
        assert_eq!(parts.len(), 1);
        parts = fold_event_into_parts(
            &parts,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        parts = fold_event_into_parts(&parts, &text_delta("after"));
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            MessagePart::Text { text, .. } => assert_eq!(text, "after"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_started_resets_accumulator() {
        let parts = fold_event_into_parts(&[], &text_delta("junk"));
        let reset = fold_event_into_parts(
            &parts,
            &AgentEvent::SessionStarted {
                harness: comet_proto::HarnessId::Mock,
                model: "m".into(),
                tools: vec![],
                cwd: "/".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
        );
        assert!(reset.is_empty());
    }

    #[test]
    fn tool_call_refresh_is_idempotent() {
        let call = AgentEvent::ToolCall {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        };
        let once = fold_event_into_parts(&[], &call);
        let twice = fold_event_into_parts(&once, &call);
        assert_eq!(once, twice);
    }

    #[test]
    fn tool_result_marks_resolution() {
        let mut parts = fold_event_into_parts(
            &[],
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        parts = fold_event_into_parts(
            &parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                is_error, resolved, ..
            } => {
                assert!(*is_error);
                assert!(*resolved);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitize_strips_heavy_inputs_and_is_idempotent() {
        let call = ToolCall::WriteFile {
            path: "/x".into(),
            content: Some("secret".into()),
        };
        let clean = sanitize_tool_call(&call);
        assert_eq!(
            clean,
            ToolCall::WriteFile {
                path: "/x".into(),
                content: None
            }
        );
        assert_eq!(sanitize_tool_call(&clean), clean);
    }

    #[test]
    fn split_and_join_round_trip() {
        let big = "x".repeat(MSG_INLINE_MAX * 2 + 100);
        let parts = vec![
            MessagePart::Text {
                id: "t0".into(),
                text: big.clone(),
            },
            MessagePart::Tool {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
            },
        ];
        let chunks = split_parts(&parts);
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|p| p.byte_len()).sum();
            assert!(bytes <= MSG_INLINE_MAX, "chunk over cap: {bytes}");
        }
        let joined = join_continuations(chunks);
        let text: String = joined
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, big);
        assert!(matches!(joined.last().unwrap(), MessagePart::Tool { .. }));
    }

    #[test]
    fn continuation_ids_are_deterministic() {
        assert_eq!(continuation_id("m1", 1), "m1#c1");
    }
}
