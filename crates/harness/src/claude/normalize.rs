//! Frame → [`AgentEvent`] normalization (init dedupe, subagent tagging, tool
//! decoding, error-code mapping).

use serde_json::Value;
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, TodoItem, ToolCall};

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

/// The CLI seeds `result.errors` with internal `[ede_diagnostic]` breadcrumbs
/// for its error_during_execution telemetry ("turn aborted (…) stop_reason=…",
/// "result_type=… last_content_type=… stop_reason=…"). They're diagnostics
/// about the CLI's own turn accounting, not user-relevant errors — surfacing
/// them verbatim put raw `[ede_diagnostic] result_type=user …` boxes in the
/// transcript. They're debug-logged and dropped instead.
fn is_internal_diagnostic(message: &str) -> bool {
    message.contains("[ede_diagnostic]")
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
        // The subagent spawn: name the chip — and the tab it opens — after
        // the TASK, not the bare tool ("Agent" alone says nothing in a tab
        // strip; the tool's `description` is where the work is named).
        "Agent" | "Task" => {
            let description = str_field(input, "description");
            ToolCall::Unknown {
                name: if description.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {description}")
                },
                input: (!input.is_null()).then(|| input.clone()),
            }
        }
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

/// Wrap an event as subagent-attributed traffic.
fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

/// CLI-synthesized text that rides a user frame but is NOT conversation:
/// `<system-reminder>` context injections and the interruption marker the CLI
/// stamps into the transcript when a turn (or a subagent) is stopped.
///
/// On a TAGGED frame the distinction is load-bearing, not cosmetic. A tagged
/// user message means "the parent steered its subagent", which announces more
/// work and is therefore the one event allowed to resurrect a settled spawn
/// chip. The CLI emits `[Request interrupted by user]` on the child feed
/// immediately AFTER the subagent's `done{interrupted}` — read as a steer it
/// un-settled a chip that nothing would ever settle again, and the spinner ran
/// forever (2026-08-21: "orchestrator killed them but spinner doesn't stop").
/// It announces the opposite of more work.
///
/// Prefix-matched: the CLI ships at least two spellings of the marker
/// (`…by user]` and `…by user for tool use]`).
fn is_synthetic_user_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<system-reminder>") || text.starts_with("[Request interrupted")
}

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init` — the CLI re-emits it every time the model
/// is re-invoked WITHIN one session (a background-subagent wake turn, a
/// scheduled wakeup), not just at start (live-verified against 2.1.228: a
/// background subagent finishing produces a second init with the SAME session
/// id, then a second `result`). Downstream, `SessionStarted` is the fold's run
/// boundary (it resets accumulated parts), so one run ⇒ one `SessionStarted`;
/// the wake turn's own frames flow through and the engine's parked-session
/// resume turns them into the done→Working→done wake.
pub(crate) struct Normalizer {
    saw_init: bool,
    last_model: Option<String>,
    /// Background-agent ids (`task_started.task_id`) → the spawning Agent
    /// tool_use id. `SendMessage` steers address the AGENT id; this map
    /// re-keys them onto the spawn chip's feed (the wire never echoes the
    /// steer on the child feed — live-verified 2.1.228).
    agent_tasks: std::collections::HashMap<String, String>,
    /// tool_use ids of Agent/Task spawn calls, recorded from their own
    /// assistant frames (plus `task_started`'s agent-task pairing). Gates
    /// `task_notification`: background SHELL tasks settle through the same
    /// subtype carrying their Bash call's id, and tagging that Done as
    /// subagent traffic stamped a spawn ref onto an ordinary Run chip —
    /// which then opened as an empty, never-created subagent doc (user
    /// report 2026-08-20).
    agent_spawn_tools: std::collections::HashSet<String>,
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
            last_model: None,
            agent_tasks: std::collections::HashMap::new(),
            agent_spawn_tools: std::collections::HashSet::new(),
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
                // A background subagent's completion arrives as an UNTAGGED
                // `task_notification` carrying the spawning tool's id — the
                // wire's only terminal signal for it (live-verified 2.1.228:
                // no tagged frame follows; the subagent's stream just stops).
                // Surface it as the subagent's tagged Done so the chip flips
                // done/failed and the transcript freezes.
                if f.subtype == "task_notification" {
                    let Some(parent) = f.tool_use_id.as_deref().filter(|t| !t.is_empty()) else {
                        return Vec::new();
                    };
                    // Only a KNOWN spawn settles as a subagent. Background
                    // SHELL tasks (`Bash` with `run_in_background`) settle
                    // through this same subtype carrying the Bash call's id —
                    // tagging that Done would bind a subagent ref onto an
                    // ordinary Run chip. A real spawn's tool_use frame always
                    // precedes its notification on the wire, so the set is
                    // populated by the time a genuine one arrives.
                    if !self.agent_spawn_tools.contains(parent) {
                        return Vec::new();
                    }
                    let status = match f.status.as_deref().unwrap_or("") {
                        "completed" | "complete" | "succeeded" | "success" => {
                            DoneStatus::Completed
                        }
                        "failed" | "errored" | "error" => DoneStatus::Errored,
                        "killed" | "cancelled" | "canceled" | "stopped" | "interrupted" => {
                            DoneStatus::Interrupted
                        }
                        // Non-terminal notification shapes: nothing to close.
                        _ => return Vec::new(),
                    };
                    return vec![tag(
                        parent,
                        AgentEvent::Done {
                            status,
                            result: None,
                            error: None,
                            session_id: None,
                        },
                    )];
                }
                // An AGENT task starting (subagent_type present — subagent-
                // owned shell tasks carry the same subtype without it):
                // record agentId → spawn id for SendMessage steer re-keying.
                if f.subtype == "task_started"
                    && f.subagent_type.is_some()
                    && let (Some(task), Some(tool)) = (
                        f.task_id.as_deref().filter(|t| !t.is_empty()),
                        f.tool_use_id.as_deref().filter(|t| !t.is_empty()),
                    )
                {
                    self.agent_tasks.insert(task.to_owned(), tool.to_owned());
                    self.agent_spawn_tools.insert(tool.to_owned());
                    return Vec::new();
                }
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
            // nested transcript: a background subagent runs concurrently with
            // the parent's own stream, so folding them into the parent feed
            // would split a contiguous text block around phantom tool calls
            // (and a subagent's message boundaries must never rotate the
            // parent's assistant message id). They are wrapped in
            // `AgentEvent::Subagent` instead — the engine routes them to the
            // subagent's own doc.
            Frame::StreamEvent(f) => {
                if f.event.kind != "content_block_delta" {
                    return Vec::new();
                }
                if let Some(parent) = &f.parent_tool_use_id {
                    return match f.event.delta.kind.as_str() {
                        "text_delta" => vec![tag(
                            parent,
                            AgentEvent::TextDelta {
                                text: f.event.delta.text,
                            },
                        )],
                        "thinking_delta" if !f.event.delta.thinking.is_empty() => vec![tag(
                            parent,
                            AgentEvent::ReasoningDelta {
                                text: f.event.delta.thinking,
                            },
                        )],
                        // Subagent liveness heartbeats have no consumer; the
                        // parent turn is already done (eager-done policy).
                        _ => Vec::new(),
                    };
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" => vec![AgentEvent::TextDelta {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" => vec![AgentEvent::ReasoningDelta {
                        text: f.event.delta.thinking,
                    }],
                    // A big tool input (a 90-line Write) streams as a long run
                    // of input_json_delta frames with nothing else — minutes of
                    // apparent silence that reads as a stalled run. Surface
                    // them as empty reasoning deltas: the engine treats those
                    // as pure liveness heartbeats (never journaled/rendered).
                    "input_json_delta" => vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                if let Some(parent) = &f.parent_tool_use_id {
                    // Subagent content, attributed. The 2.1.x wire streams NO
                    // tagged partial deltas (live-verified): a subagent's text
                    // arrives only as full text blocks on its tagged
                    // assistant frames — emit them, in block order with the
                    // tool calls, or subagent transcripts are tool-chips-only.
                    let mut out: Vec<AgentEvent> = f
                        .message
                        .blocks()
                        .filter_map(|b: ContentBlock| match b.kind.as_str() {
                            "text" if !b.text.is_empty() => Some(tag(
                                parent,
                                AgentEvent::TextDelta {
                                    text: format!("{}\n\n", b.text.trim_end()),
                                },
                            )),
                            "tool_use" => Some(tag(
                                parent,
                                AgentEvent::ToolCall {
                                    id: b.id.clone(),
                                    call: decode_tool_use(&b.name, &b.input),
                                },
                            )),
                            _ => None,
                        })
                        .collect();
                    if let Some(code) = &f.error {
                        out.push(tag(
                            parent,
                            AgentEvent::Error {
                                message: assistant_error_text(code),
                            },
                        ));
                    }
                    return out;
                }
                // Record spawn tool ids up front: `task_notification` keys on
                // them, and only foreground spawns ever get a `task_started`.
                for b in f.message.blocks() {
                    if b.kind == "tool_use" && matches!(b.name.as_str(), "Agent" | "Task") {
                        self.agent_spawn_tools.insert(b.id.clone());
                    }
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .flat_map(|b| {
                        let call = AgentEvent::ToolCall {
                            id: b.id.clone(),
                            call: decode_tool_use(&b.name, &b.input),
                        };
                        // A spawn's `prompt` is the subagent's opening user
                        // message — the wire never echoes it on the child
                        // feed (child user frames carry tool results and
                        // steers only), so seed it here and the subagent
                        // transcript starts the way every chat does.
                        let opening = matches!(b.name.as_str(), "Agent" | "Task")
                            .then(|| b.input.get("prompt"))
                            .flatten()
                            .and_then(Value::as_str)
                            .filter(|p| !p.trim().is_empty())
                            .map(|prompt| tag(
                                &b.id,
                                AgentEvent::UserMessage {
                                    text: prompt.to_owned(),
                                },
                            ));
                        // A SendMessage steer never echoes on the child feed
                        // (live-verified) — surface it from the parent's own
                        // call, re-keyed onto the spawn it addresses.
                        let steer = (b.name == "SendMessage")
                            .then(|| {
                                let to = ["to", "recipient"]
                                    .iter()
                                    .find_map(|k| b.input.get(*k))
                                    .and_then(Value::as_str)?;
                                let spawn = self.agent_tasks.get(to)?;
                                let text = ["message", "content"]
                                    .iter()
                                    .find_map(|k| b.input.get(*k))
                                    .and_then(Value::as_str)
                                    .filter(|m| !m.trim().is_empty())?;
                                Some(tag(
                                    spawn,
                                    AgentEvent::UserMessage {
                                        text: text.to_owned(),
                                    },
                                ))
                            })
                            .flatten();
                        std::iter::once(call).chain(opening).chain(steer)
                    })
                    .collect();
                self.last_model = f.message.model.clone().or(self.last_model.take());
                if let Some(usage) = &f.message.usage {
                    let fields = [
                        "input_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ];
                    if fields
                        .iter()
                        .any(|key| usage.get(*key).and_then(Value::as_u64).is_some())
                    {
                        let tokens = fields
                            .iter()
                            .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
                            .fold(0u64, u64::saturating_add);
                        out.push(AgentEvent::ContextUsage {
                            tokens: Some(tokens),
                            window: None,
                        });
                    }
                }
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
                if let Some(parent) = &f.parent_tool_use_id {
                    // A subagent's tool results echo on the main channel too;
                    // they belong to its transcript, attributed like its calls.
                    let mut out: Vec<AgentEvent> = f
                        .message
                        .blocks()
                        .filter(|b: &ContentBlock| b.kind == "tool_result")
                        .map(|b| {
                            tag(
                                parent,
                                AgentEvent::ToolResult {
                                    id: b.tool_use_id.clone(),
                                    is_error: b.is_error.unwrap_or(false),
                                    output: None,
                                    diff: None,
                                },
                            )
                        })
                        .collect();
                    // A tagged user frame's TEXT blocks are the parent
                    // steering its subagent (SendMessage-style follow-ups —
                    // tool results ride their own blocks, filtered above).
                    // Synthetic harness injections are not conversation, and
                    // must not read as a steer — see [`is_synthetic_user_text`].
                    out.extend(
                        f.message
                            .blocks()
                            .filter(|b: &ContentBlock| {
                                b.kind == "text"
                                    && !b.text.trim().is_empty()
                                    && !is_synthetic_user_text(&b.text)
                            })
                            .map(|b| tag(parent, AgentEvent::UserMessage { text: b.text })),
                    );
                    return out;
                }
                f.message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_result")
                    .map(|b| AgentEvent::ToolResult {
                        id: b.tool_use_id.clone(),
                        is_error: b.is_error.unwrap_or(false),
                        output: None,
                        diff: None,
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
                let model_usage = self
                    .last_model
                    .as_ref()
                    .and_then(|model| {
                        f.model_usage.get(model).or_else(|| {
                            f.model_usage.values().find(|entry| {
                                entry.get("canonicalModel").and_then(Value::as_str)
                                    == Some(model.as_str())
                            })
                        })
                    })
                    .or_else(|| {
                        (f.model_usage.len() == 1)
                            .then(|| f.model_usage.values().next())
                            .flatten()
                    });
                let window = model_usage
                    .and_then(|entry| entry.get("contextWindow"))
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0);
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
                    // Split the CLI's internal `[ede_diagnostic]` breadcrumbs
                    // off the real errors: diagnostics are debug-logged, never
                    // surfaced as transcript error parts.
                    let (diagnostics, errors): (Vec<String>, Vec<String>) = f
                        .errors
                        .iter()
                        .map(|e| match e {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .partition(|m| is_internal_diagnostic(m));
                    for diagnostic in &diagnostics {
                        tracing::debug!(
                            target: "zeron_harness::claude",
                            "internal CLI diagnostic (not surfaced): {diagnostic}"
                        );
                    }
                    let error = if !errors.is_empty() {
                        // Real user-relevant errors — surface verbatim.
                        Some(errors.join("; "))
                    } else {
                        match f.subtype.as_str() {
                            // Known run-failure subtypes stay visible with
                            // their mapped human wording (never blank — a
                            // blank error folds to no part and the failed
                            // turn reads as a silent non-reply).
                            "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries" => {
                                Some(result_error_text(&f.subtype).to_owned())
                            }
                            // Diagnostic-only ends (the CLI's turn-accounting
                            // telemetry, typically `error_during_execution`
                            // after an abort): nothing user-relevant to show.
                            _ if !diagnostics.is_empty() => None,
                            _ => Some(result_error_text(&f.subtype).to_owned()),
                        }
                    };
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error,
                        session_id: f.session_id,
                    }
                };
                let mut out = Vec::new();
                if let Some(window) = window {
                    out.push(AgentEvent::ContextUsage {
                        tokens: None,
                        window: Some(window),
                    });
                }
                out.extend([usage, done]);
                out
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

    fn normalize_one(raw: &str) -> Vec<AgentEvent> {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        Normalizer::new().normalize(frame, false)
    }

    fn result_done(raw: &str) -> AgentEvent {
        let events = normalize_one(raw);
        assert_eq!(events.len(), 2, "usage + done");
        events.into_iter().nth(1).expect("done event")
    }

    #[test]
    fn stream_deltas_map_to_text_reasoning_and_heartbeats() {
        // Real thinking text streams as a reasoning delta.
        let ev = normalize_one(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
        );
        assert_eq!(ev, vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]);
        // Redacted thinking (estimated_tokens only) yields the empty
        // heartbeat shape the engine filters.
        let ev = normalize_one(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // A tool input being generated (input_json_delta) is a liveness
        // heartbeat, not silence — minutes of a big Write must not read as
        // a stalled run.
        let ev = normalize_one(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"file_"}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // Signature deltas stay dropped.
        let ev = normalize_one(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"abc"}}}"#,
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn subagent_stream_deltas_arrive_tagged() {
        let ev = normalize_one(
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_sub","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub text"}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "toolu_sub".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "sub text".into()
                }),
            }]
        );
        // Subagent input_json heartbeats are dropped — the parent turn is
        // already done under the eager-done policy, nothing to keep alive.
        let ev = normalize_one(
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_sub","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn subagent_tool_calls_and_results_arrive_tagged_without_boundary_rotation() {
        let mut norm = Normalizer::new();
        let before = norm.assistant_message_id.clone();
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        )
        .expect("parses");
        let ev = norm.normalize(frame, false);
        assert_eq!(
            ev,
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "toolu_sub".into(),
                event: Box::new(AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::Exec {
                        command: "ls".into()
                    },
                }),
            }]
        );
        // A subagent's assistant frame must NOT rotate the parent's message
        // id (it would split the parent's contiguous text mid-stream).
        assert_eq!(norm.assistant_message_id, before);

        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false}]}}"#,
        )
        .expect("parses");
        let ev = norm.normalize(frame, false);
        assert_eq!(
            ev,
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "toolu_sub".into(),
                event: Box::new(AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false,
                    output: None,
                    diff: None,
                }),
            }]
        );
    }

    #[test]
    fn spawn_prompt_seeds_the_subagent_opening_user_message() {
        // The wire never echoes a Task's prompt on the child feed, so the
        // spawn itself seeds the subagent's opening user entry.
        let ev = normalize_one(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_sub","name":"Task","input":{"description":"probe","prompt":"scan the fold path"}}]}}"#,
        );
        assert!(matches!(
            &ev[..],
            [
                AgentEvent::ToolCall { id, .. },
                AgentEvent::Subagent { parent_tool_use_id, event },
                AgentEvent::AssistantMessageCompleted { .. },
            ] if id == "toolu_sub"
                && parent_tool_use_id == "toolu_sub"
                && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "scan the fold path")
        ));
        // No prompt → no synthetic opening; ordinary tools never spawn one.
        for frame in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Task","input":{"description":"probe"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"ls","prompt":"red herring"}}]}}"#,
        ] {
            let ev = normalize_one(frame);
            assert!(
                !ev.iter()
                    .any(|e| matches!(e, AgentEvent::Subagent { .. })),
                "{frame}: {ev:?}"
            );
        }
    }

    /// Killing a subagent puts `done{interrupted}` on the child feed and then
    /// an interruption MARKER as a tagged user frame. Read as a steer, that
    /// marker resurrects the spawn chip the `done` just settled — and nothing
    /// ever settles it again, so the chip spins forever. It is CLI
    /// bookkeeping, filtered like a `<system-reminder>`; a real steer on the
    /// same frame shape still gets through.
    #[test]
    fn the_interruption_marker_is_not_a_steer() {
        for marker in [
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
        ] {
            let frame = format!(
                r#"{{"type":"user","parent_tool_use_id":"toolu_spawn","message":{{"content":[{{"type":"text","text":"{marker}"}}]}}}}"#
            );
            assert!(
                !normalize_one(&frame)
                    .iter()
                    .any(|e| matches!(e, AgentEvent::Subagent { .. })),
                "{marker} leaked as a steer"
            );
        }
        // A genuine steer on the very same frame shape still arrives.
        let real = r#"{"type":"user","parent_tool_use_id":"toolu_spawn","message":{"content":[{"type":"text","text":"Keep going."}]}}"#;
        assert!(
            normalize_one(real).iter().any(|e| matches!(
                e,
                AgentEvent::Subagent { parent_tool_use_id, event }
                    if parent_tool_use_id == "toolu_spawn"
                        && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "Keep going.")
            )),
            "a real steer must still reach the subagent"
        );
    }

    #[test]
    fn send_message_steers_rekey_onto_the_spawn_feed() {
        // Live 2.1.228: the steer NEVER echoes on the child feed; the only
        // wire evidence is the parent's SendMessage call addressed to the
        // agent id that task_started paired with the spawn tool id.
        let mut norm = Normalizer::new();
        let started = crate::claude::wire::parse_frame(
            r#"{"type":"system","subtype":"task_started","task_id":"a20b2336","tool_use_id":"toolu_spawn","subagent_type":"general-purpose","prompt":"p","description":"d"}"#,
        )
        .expect("parses");
        assert!(norm.normalize(started, false).is_empty());
        let send = crate::claude::wire::parse_frame(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_send","name":"SendMessage","input":{"to":"a20b2336","message":"Also read the rebuild.","summary":"s"}}]}}"#,
        )
        .expect("parses");
        let ev = norm.normalize(send, false);
        assert!(
            ev.iter().any(|e| matches!(
                e,
                AgentEvent::Subagent { parent_tool_use_id, event }
                    if parent_tool_use_id == "toolu_spawn"
                        && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "Also read the rebuild.")
            )),
            "{ev:?}"
        );
        // Unknown recipient (no task_started seen) or a subagent-owned shell
        // task's task_started: no steer synthesized.
        let mut norm = Normalizer::new();
        let shell_task = crate::claude::wire::parse_frame(
            r#"{"type":"system","subtype":"task_started","task_id":"bg1","tool_use_id":"toolu_bash","task_type":"local_bash"}"#,
        )
        .expect("parses");
        assert!(norm.normalize(shell_task, false).is_empty());
        let send = crate::claude::wire::parse_frame(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"SendMessage","input":{"to":"bg1","message":"x"}}]}}"#,
        )
        .expect("parses");
        assert!(
            !norm
                .normalize(send, false)
                .iter()
                .any(|e| matches!(e, AgentEvent::Subagent { .. }))
        );
    }

    #[test]
    fn tagged_user_text_becomes_a_subagent_steer() {
        // A tagged user frame's TEXT block is the parent steering its
        // subagent — forwarded as a tagged UserMessage so the subagent doc
        // grows a user entry.
        let ev = normalize_one(
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"text","text":"Also check the rebuild path."}]}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "toolu_sub".into(),
                event: Box::new(AgentEvent::UserMessage {
                    text: "Also check the rebuild path.".into(),
                }),
            }]
        );
        // Synthetic harness injections are not conversation, and blank text
        // is noise; an UNTAGGED user text frame is not a steer at all (the
        // parent chat's user messages come from doc commands).
        for frame in [
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"text","text":"<system-reminder>tick</system-reminder>"}]}}"#,
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"text","text":"   "}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"typed into the parent"}]}}"#,
        ] {
            assert_eq!(normalize_one(frame), Vec::new(), "frame: {frame}");
        }
        // Mixed frames keep both: the tool result AND the steer text.
        let ev = normalize_one(
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false},{"type":"text","text":"Keep going."}]}}"#,
        );
        assert!(matches!(
            &ev[..],
            [
                AgentEvent::Subagent { event: first, .. },
                AgentEvent::Subagent { event: second, .. },
            ] if matches!(first.as_ref(), AgentEvent::ToolResult { .. })
                && matches!(second.as_ref(), AgentEvent::UserMessage { text } if text == "Keep going.")
        ));
    }

    #[test]
    fn task_notification_settles_the_subagent_with_a_tagged_done() {
        // The wire's ONLY terminal signal for a background subagent
        // (live-verified 2.1.228): an untagged system frame carrying the
        // spawning tool's id. Shape from the captured fixture. The spawn's
        // own tool_use frame always precedes it — that's what marks the id
        // as an AGENT task (shell tasks share the subtype).
        let spawn = |norm: &mut Normalizer| {
            let frame = crate::claude::wire::parse_frame(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"probe"}}]}}"#,
            )
            .expect("parses");
            norm.normalize(frame, false);
        };
        let notify = |norm: &mut Normalizer, raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("parses");
            norm.normalize(frame, false)
        };
        let mut norm = Normalizer::new();
        spawn(&mut norm);
        let ev = notify(
            &mut norm,
            r#"{"type":"system","subtype":"task_notification","task_id":"t1","tool_use_id":"toolu_agent","status":"completed","summary":"DONE."}"#,
        );
        assert!(matches!(
            &ev[..],
            [AgentEvent::Subagent { parent_tool_use_id, event }]
                if parent_tool_use_id == "toolu_agent"
                    && matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Completed, .. })
        ));
        let mut norm = Normalizer::new();
        spawn(&mut norm);
        let ev = notify(
            &mut norm,
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"toolu_agent","status":"failed"}"#,
        );
        assert!(matches!(
            &ev[..],
            [AgentEvent::Subagent { event, .. }]
                if matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Errored, .. })
        ));
        // Non-terminal or id-less notifications close nothing.
        let mut norm = Normalizer::new();
        spawn(&mut norm);
        assert!(notify(
            &mut norm,
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"toolu_agent","status":"running"}"#,
        )
        .is_empty());
        assert!(normalize_one(
            r#"{"type":"system","subtype":"task_notification","status":"completed"}"#,
        )
        .is_empty());
    }

    #[test]
    fn shell_task_notification_never_settles_a_subagent() {
        // `Bash` with `run_in_background` settles through the SAME
        // `task_notification` subtype, carrying the Bash call's own id.
        // Tagging that Done bound a subagent ref onto an ordinary Run chip,
        // which then rendered as a spawn chip opening an empty, never-created
        // subagent doc (user report 2026-08-20).
        let mut norm = Normalizer::new();
        for raw in [
            // The shell task's start is already unmapped (no subagent_type)…
            r#"{"type":"system","subtype":"task_started","task_id":"bg1","tool_use_id":"toolu_bash","task_type":"local_bash"}"#,
            // …and the Bash call itself must not mark the id as a spawn.
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"git clone …","run_in_background":true}}]}}"#,
        ] {
            let frame = crate::claude::wire::parse_frame(raw).expect("parses");
            norm.normalize(frame, false);
        }
        let done = crate::claude::wire::parse_frame(
            r#"{"type":"system","subtype":"task_notification","task_id":"bg1","tool_use_id":"toolu_bash","status":"completed"}"#,
        )
        .expect("parses");
        assert!(norm.normalize(done, false).is_empty());
    }

    #[test]
    fn subagent_assistant_text_blocks_emit_tagged_text() {
        // No tagged partial deltas exist on the 2.1.x wire: a subagent's text
        // arrives only as full blocks on its tagged assistant frames.
        let ev = normalize_one(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"text","text":"working on it"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        assert_eq!(ev.len(), 2);
        assert_eq!(
            ev[0],
            AgentEvent::Subagent {
                parent_tool_use_id: "toolu_sub".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "working on it\n\n".into()
                }),
            }
        );
        assert!(matches!(&ev[1], AgentEvent::Subagent { event, .. }
            if matches!(event.as_ref(), AgentEvent::ToolCall { .. })));
    }

    #[test]
    fn wake_turn_init_is_deduped_but_second_result_still_emits_done() {
        // Live-verified 2.1.228 shape: background subagent → eager result,
        // then a second init (same session id) + second result on completion.
        let mut norm = Normalizer::new();
        let init = r#"{"type":"system","subtype":"init","model":"m","cwd":"/x","session_id":"s1"}"#;
        let frame = crate::claude::wire::parse_frame(init).unwrap();
        assert_eq!(norm.normalize(frame, false).len(), 1, "first init");
        let frame = crate::claude::wire::parse_frame(init).unwrap();
        assert!(
            norm.normalize(frame, false).is_empty(),
            "wake init deduped — SessionStarted is the fold's run boundary"
        );
        let result = r#"{"type":"result","subtype":"success","session_id":"s1"}"#;
        let frame = crate::claude::wire::parse_frame(result).unwrap();
        let events = norm.normalize(frame, false);
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                })
            ),
            "wake turn settles with its own Done"
        );
    }

    #[test]
    fn ede_diagnostics_never_surface_as_errors() {
        // The CLI's internal turn-accounting breadcrumbs must not become
        // transcript error parts (they showed up as raw red boxes).
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(status, DoneStatus::Errored);
                assert_eq!(error, None, "diagnostic-only failure surfaces no text");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn real_errors_survive_diagnostic_filtering() {
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] turn aborted (x) stop_reason=null","Something real broke"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(error.as_deref(), Some("Something real broke"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn known_failure_subtypes_keep_mapped_wording() {
        // A known run-failure subtype stays visible with human wording even
        // when its errors array is all diagnostics (or empty).
        let done = result_done(
            r#"{"type":"result","subtype":"error_max_turns","errors":["[ede_diagnostic] turn aborted (max) stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let done = result_done(r#"{"type":"result","subtype":"error_max_turns","errors":[]}"#);
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    #[test]
    fn context_counts_cached_prompt_and_matches_primary_model() {
        let mut normalizer = Normalizer::new();
        let events = normalizer.normalize(super::super::wire::parse_frame(r#"{"type":"assistant","message":{"model":"primary","content":[],"usage":{"input_tokens":200,"cache_read_input_tokens":40000,"cache_creation_input_tokens":1800,"output_tokens":100}}}"#).unwrap(), false);
        assert!(events.contains(&AgentEvent::ContextUsage {
            tokens: Some(42000),
            window: None
        }));
        let events = normalizer.normalize(super::super::wire::parse_frame(r#"{"type":"assistant","parent_tool_use_id":"child","message":{"model":"child","content":[],"usage":{"input_tokens":999999}}}"#).unwrap(), false);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ContextUsage { .. }))
        );
        let events = normalizer.normalize(super::super::wire::parse_frame(r#"{"type":"result","subtype":"success","usage":{"input_tokens":999999},"modelUsage":{"primary":{"contextWindow":200000},"child":{"contextWindow":1000000}}}"#).unwrap(), false);
        assert!(events.contains(&AgentEvent::ContextUsage {
            tokens: None,
            window: Some(200000)
        }));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::ContextUsage {
                tokens: Some(_),
                ..
            }
        )));
    }
}
