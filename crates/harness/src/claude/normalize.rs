//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use comet_proto::{AgentEvent, DoneStatus, HarnessId, TodoItem, ToolCall};
use serde_json::Value;

use super::wire::{ContentBlock, Frame};

/// Human-readable text for the CLI's assistant-level error codes. These arrive
/// as a terse `error` field on an `assistant` frame — usually with NO text
/// content and NOT as a `result` error — so a usage-limited or otherwise failed
/// turn looks like the agent simply never replied unless we surface it.
fn assistant_error_text(code: &str) -> String {
    match code {
        "authentication_failed" => "Authentication failed — sign in to Claude again.".into(),
        "oauth_org_not_allowed" => "This organization isn't allowed to use Claude here.".into(),
        "billing_error" => "Billing error — check your Claude plan or payment method.".into(),
        "rate_limit" => "Claude usage limit reached — try again after the limit resets.".into(),
        "overloaded" => "Claude is overloaded right now — try again shortly.".into(),
        "invalid_request" => "The request was rejected as invalid.".into(),
        "model_not_found" => "The selected model isn't available.".into(),
        "server_error" => "Claude had a server error — try again.".into(),
        "max_output_tokens" => "The reply hit the maximum output length.".into(),
        "unknown" => "Claude returned an unspecified error.".into(),
        other => format!("Claude error: {other}"),
    }
}

/// Which claude.ai usage window a `rate_limit_event` refers to.
fn rate_window_label(kind: &str) -> &'static str {
    match kind {
        "five_hour" => "5-hour",
        "seven_day" | "seven_day_overage_included" => "weekly",
        "seven_day_opus" => "weekly (Opus)",
        "seven_day_sonnet" => "weekly (Sonnet)",
        "overage" => "overage",
        _ => "usage",
    }
}

/// Fallback wording for a `result` error whose `errors` array is empty, so the
/// turn never ends with a blank (and therefore invisible) error.
fn result_error_text(subtype: &str) -> &'static str {
    match subtype {
        "error_max_turns" => "The run hit the maximum number of turns.",
        "error_max_budget_usd" => "The run hit its cost budget.",
        "error_max_structured_output_retries" => "The run exhausted its structured-output retries.",
        _ => "The run ended with an error.",
    }
}

fn str_field(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").into()
}

fn opt_str_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Decode a Claude `tool_use` block (name + input) into a typed [`ToolCall`].
pub(crate) fn decode_tool_use(name: &str, input: &Value) -> ToolCall {
    match name {
        "Bash" => ToolCall::Exec {
            command: str_field(input, "command"),
        },
        "Read" => ToolCall::ReadFile {
            path: str_field(input, "file_path"),
        },
        "Write" => ToolCall::WriteFile {
            path: str_field(input, "file_path"),
            content: opt_str_field(input, "content"),
        },
        "Edit" => ToolCall::EditFile {
            path: str_field(input, "file_path"),
            old_string: opt_str_field(input, "old_string"),
            new_string: opt_str_field(input, "new_string"),
        },
        "Grep" => ToolCall::Search {
            pattern: str_field(input, "pattern"),
            path: opt_str_field(input, "path"),
        },
        "Glob" => ToolCall::Glob {
            pattern: str_field(input, "pattern"),
        },
        "WebFetch" => ToolCall::WebFetch {
            url: str_field(input, "url"),
            prompt: opt_str_field(input, "prompt"),
        },
        "WebSearch" => ToolCall::WebSearch {
            query: str_field(input, "query"),
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, "content"),
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // MCP tools arrive as `mcp__<server>__<tool>`.
        _ => match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, tool)) => ToolCall::Mcp {
                server: server.into(),
                tool: tool.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
            None => ToolCall::Unknown {
                name: name.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
        },
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init` — the CLI re-emits it every time the model
/// is re-invoked WITHIN one session (a background-task notification, a
/// scheduled wakeup), not just at start. Downstream, `SessionStarted` is the
/// fold's run boundary (it resets accumulated parts), so one run ⇒ one
/// `SessionStarted`.
pub(crate) struct Normalizer {
    saw_init: bool,
    /// Rotates at each assistant-frame close and at each steer; SessionStarted
    /// carries the first value so folds can attribute deltas from the start.
    assistant_message_id: String,
    /// Last session id seen (init or result) — used for synthetic Dones.
    pub session_id: Option<String>,
}

impl Normalizer {
    pub fn new() -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
        }
    }

    /// Rotate the assistant message id for a steer boundary; returns
    /// (previous, next) for the `Steered` event.
    pub fn rotate_for_steer(&mut self) -> (String, String) {
        let prev = std::mem::replace(&mut self.assistant_message_id, new_message_id());
        (prev, self.assistant_message_id.clone())
    }

    /// Normalize one stdout frame into 0+ unified events. `interrupted` folds
    /// a post-interrupt `result` into `Done { status: Interrupted }`.
    pub fn normalize(&mut self, frame: Frame, interrupted: bool) -> Vec<AgentEvent> {
        match frame {
            Frame::System(f) => {
                if f.subtype != "init" || self.saw_init {
                    return Vec::new();
                }
                self.saw_init = true;
                self.session_id = Some(f.session_id.clone());
                vec![AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: f.model,
                    tools: f.tools,
                    cwd: f.cwd,
                    session_id: f.session_id,
                    assistant_message_id: self.assistant_message_id.clone(),
                }]
            }

            // Frames with `parent_tool_use_id` set belong to a SUBAGENT's
            // nested transcript; a background Task runs concurrently with the
            // parent's text stream, so folding them in would split a contiguous
            // text block around a phantom tool call. Only null-parent frames
            // are this turn's own content.
            Frame::StreamEvent(f) => {
                if f.parent_tool_use_id.is_some() || f.event.kind != "content_block_delta" {
                    return Vec::new();
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" => vec![AgentEvent::TextDelta {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" => vec![AgentEvent::ReasoningDelta {
                        text: f.event.delta.thinking,
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .map(|b| AgentEvent::ToolCall {
                        id: b.id.clone(),
                        call: decode_tool_use(&b.name, &b.input),
                    })
                    .collect();
                // A failed turn (usage limit, billing, auth, overloaded, …)
                // carries a terse `error` code here — often with empty content
                // and no `result` error — so surface it visibly.
                if let Some(code) = &f.error {
                    out.push(AgentEvent::Error {
                        message: assistant_error_text(code),
                    });
                }
                // The enclosing assistant frame closes the streamed message
                // item; rotate so post-boundary deltas get a fresh id.
                let (prev, _next) = self.rotate_for_steer();
                out.push(AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: prev,
                });
                out
            }

            Frame::User(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                f.message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_result")
                    .map(|b| AgentEvent::ToolResult {
                        id: b.tool_use_id.clone(),
                        is_error: b.is_error.unwrap_or(false),
                    })
                    .collect()
            }

            // A claude.ai plan window was hit. A hard `rejected` blocks the
            // turn — make it visible; allowed/allowed_warning stay quiet.
            Frame::RateLimit(f) => {
                if f.rate_limit_info.status != "rejected" {
                    return Vec::new();
                }
                let window =
                    rate_window_label(f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""));
                vec![AgentEvent::Error {
                    message: format!(
                        "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                    ),
                }]
            }

            Frame::Result(f) => {
                if let Some(id) = &f.session_id {
                    self.session_id = Some(id.clone());
                }
                let usage = AgentEvent::Usage {
                    input_tokens: f.usage.input_tokens,
                    output_tokens: f.usage.output_tokens,
                };
                let done = if f.subtype == "success" {
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Completed
                        },
                        result: f.result,
                        error: None,
                        session_id: f.session_id,
                    }
                } else {
                    // Never let the terminal error be blank: an empty `errors`
                    // array would fold to no error part and the failed turn
                    // would read as a silent non-reply.
                    let errors: Vec<String> = f
                        .errors
                        .iter()
                        .map(|e| match e {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    let error = if errors.is_empty() {
                        result_error_text(&f.subtype).to_owned()
                    } else {
                        errors.join("; ")
                    };
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error: Some(error),
                        session_id: f.session_id,
                    }
                };
                vec![usage, done]
            }

            // Control frames are handled by the run loop, not normalized.
            Frame::ControlRequest(_) | Frame::Other => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_typed_tools() {
        assert_eq!(
            decode_tool_use("Bash", &json!({"command": "ls -la"})),
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        assert_eq!(
            decode_tool_use(
                "Edit",
                &json!({"file_path": "/a", "old_string": "x", "new_string": "y"})
            ),
            ToolCall::EditFile {
                path: "/a".into(),
                old_string: Some("x".into()),
                new_string: Some("y".into())
            }
        );
        assert_eq!(
            decode_tool_use(
                "TodoWrite",
                &json!({"todos": [{"content": "t", "status": "completed"}]})
            ),
            ToolCall::Todo {
                items: vec![TodoItem {
                    text: "t".into(),
                    done: true
                }]
            }
        );
        assert_eq!(
            decode_tool_use("mcp__linear__search", &json!({"q": "bug"})),
            ToolCall::Mcp {
                server: "linear".into(),
                tool: "search".into(),
                input: Some(json!({"q": "bug"}))
            }
        );
        assert!(matches!(
            decode_tool_use("Mystery", &json!({})),
            ToolCall::Unknown { .. }
        ));
    }
}
