//! Codex app-server notification/item → [`AgentEvent`] mapping, ported from
//! codex.ts's `mapItem`/notification switch.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase/snake_case item
//! types) are accepted, and unknown item types map to nothing.

use comet_proto::{AgentEvent, TodoItem, ToolCall};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Completed,
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    field(v, keys)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Delta text under either spelling the app server has used
/// (`delta` on agentMessage, `textDelta` on some reasoning builds).
pub(crate) fn delta_text(params: &Value) -> Option<String> {
    field(params, &["delta", "textDelta"])
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(crate) fn item_id(params: &Value) -> String {
    str_field(params, &["itemId", "item_id"])
}

/// `params.turn.id` on the turn/* lifecycle notifications.
pub(crate) fn turn_id(params: &Value) -> String {
    params
        .get("turn")
        .map(|t| str_field(t, &["id"]))
        .unwrap_or_default()
}

/// `params.turn.error.message` (turn/completed carries an optional error;
/// turn/failed always should).
pub(crate) fn turn_error_message(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("error"))
        .filter(|e| !e.is_null())
        .map(|e| {
            let msg = str_field(e, &["message"]);
            if msg.is_empty() { e.to_string() } else { msg }
        })
}

/// `thread/tokenUsage/updated` → a [`AgentEvent::Usage`] snapshot of the LAST
/// turn's tokens (held by the session loop, emitted before `Done`).
pub(crate) fn usage_event(params: &Value) -> Option<AgentEvent> {
    let last = field(params, &["tokenUsage", "token_usage"])?.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Some(AgentEvent::Usage {
        input_tokens: count(&["inputTokens", "input_tokens"]),
        output_tokens: count(&["outputTokens", "output_tokens"]),
    })
}

/// Tool-shaped Codex items must always close the lifecycle they open: started
/// opens the ToolCall, completed refreshes its metadata and resolves the same
/// stable id (port of codex.ts `toolLifecycle`).
fn tool_lifecycle(phase: Phase, id: String, call: ToolCall, is_error: bool) -> Vec<AgentEvent> {
    match phase {
        Phase::Started => vec![AgentEvent::ToolCall { id, call }],
        Phase::Completed => vec![
            AgentEvent::ToolCall {
                id: id.clone(),
                call,
            },
            AgentEvent::ToolResult { id, is_error },
        ],
    }
}

/// A `fileChange` item's `changes` array reduced to the typed [`ToolCall`] the
/// UI renders: a lone `add` is a file write, a lone `update` an edit, anything
/// else (deletes, multi-file changes) a patch.
fn file_change_call(changes: &[(String, String)]) -> ToolCall {
    match changes {
        [(path, kind)] if kind == "add" => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        [(path, kind)] if kind == "update" => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        [(path, _)] => ToolCall::ApplyPatch {
            path: Some(path.clone()),
        },
        _ => ToolCall::ApplyPatch { path: None },
    }
}

pub(crate) fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Map one `item/started` or `item/completed` payload's item to events.
/// `agentMessage` and `reasoning` flow through their delta channels and are
/// handled by the session loop, not here.
pub(crate) fn map_item(phase: Phase, item: &Value) -> Vec<AgentEvent> {
    let id = str_field(item, &["id"]);
    let status = str_field(item, &["status"]);
    match item_type(item) {
        "commandExecution" | "command_execution" => match phase {
            Phase::Started => vec![AgentEvent::ToolCall {
                id,
                call: ToolCall::Exec {
                    command: str_field(item, &["command"]),
                },
            }],
            Phase::Completed => {
                let exit_code = field(item, &["exitCode", "exit_code"])
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                vec![AgentEvent::ToolResult {
                    id,
                    is_error: status == "failed" || exit_code != 0,
                }]
            }
        },
        "fileChange" | "file_change" => {
            let changes: Vec<(String, String)> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    // Unknown kinds degrade to "update", like codex.ts.
                    let kind = c
                        .get("kind")
                        .and_then(Value::as_str)
                        .filter(|k| matches!(*k, "add" | "delete" | "update"))
                        .unwrap_or("update");
                    (str_field(c, &["path"]), kind.to_owned())
                })
                .collect();
            tool_lifecycle(
                phase,
                id,
                file_change_call(&changes),
                status == "failed" || status == "declined",
            )
        }
        "mcpToolCall" | "mcp_tool_call" => match phase {
            Phase::Started => {
                let input = item.get("arguments").filter(|v| !v.is_null()).cloned();
                vec![AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Mcp {
                        server: str_field(item, &["server"]),
                        tool: str_field(item, &["tool"]),
                        input,
                    },
                }]
            }
            Phase::Completed => vec![AgentEvent::ToolResult {
                id,
                is_error: status == "failed",
            }],
        },
        "webSearch" | "web_search" => tool_lifecycle(
            phase,
            id,
            ToolCall::WebSearch {
                query: str_field(item, &["query"]),
            },
            false,
        ),
        "todoList" | "todo_list" => {
            let items = item
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, &["text"]),
                    done: field(t, &["completed", "done"]).and_then(Value::as_bool) == Some(true),
                })
                .collect();
            tool_lifecycle(phase, id, ToolCall::Todo { items }, false)
        }
        "error" => vec![AgentEvent::Error {
            message: str_field(item, &["message"]),
        }],
        // userMessage / reasoning / agentMessage flow through delta channels.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delta_accepts_both_spellings() {
        assert_eq!(delta_text(&json!({"delta": "a"})), Some("a".into()));
        assert_eq!(delta_text(&json!({"textDelta": "b"})), Some("b".into()));
        assert_eq!(delta_text(&json!({"delta": ""})), None);
        assert_eq!(delta_text(&json!({})), None);
    }

    #[test]
    fn command_execution_maps_exit_code_to_error() {
        let started = map_item(
            Phase::Started,
            &json!({"type": "commandExecution", "id": "c1", "command": "ls"}),
        );
        assert_eq!(
            started,
            vec![AgentEvent::ToolCall {
                id: "c1".into(),
                call: ToolCall::Exec {
                    command: "ls".into()
                },
            }]
        );
        let completed = map_item(
            Phase::Completed,
            &json!({"type": "command_execution", "id": "c1", "status": "completed", "exit_code": 2}),
        );
        assert_eq!(
            completed,
            vec![AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn file_change_variants_map_to_typed_calls() {
        let add = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f1", "changes": [{"path": "/a.rs", "kind": "add"}]}),
        );
        assert_eq!(
            add,
            vec![AgentEvent::ToolCall {
                id: "f1".into(),
                call: ToolCall::WriteFile {
                    path: "/a.rs".into(),
                    content: None
                },
            }]
        );
        let update = map_item(
            Phase::Completed,
            &json!({"type": "fileChange", "id": "f2", "status": "declined",
                    "changes": [{"path": "/b.rs", "kind": "update"}]}),
        );
        assert_eq!(
            update,
            vec![
                AgentEvent::ToolCall {
                    id: "f2".into(),
                    call: ToolCall::EditFile {
                        path: "/b.rs".into(),
                        old_string: None,
                        new_string: None
                    },
                },
                AgentEvent::ToolResult {
                    id: "f2".into(),
                    is_error: true
                },
            ]
        );
        let multi = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f3",
                    "changes": [{"path": "/a"}, {"path": "/b", "kind": "delete"}]}),
        );
        assert_eq!(
            multi,
            vec![AgentEvent::ToolCall {
                id: "f3".into(),
                call: ToolCall::ApplyPatch { path: None },
            }]
        );
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}}})),
            Some(AgentEvent::Usage {
                input_tokens: 42,
                output_tokens: 7
            })
        );
        assert_eq!(
            usage_event(&json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}}})),
            Some(AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2
            })
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    #[test]
    fn turn_error_extraction() {
        assert_eq!(
            turn_error_message(&json!({"turn": {"id": "t", "error": {"message": "boom"}}})),
            Some("boom".into())
        );
        assert_eq!(turn_error_message(&json!({"turn": {"id": "t"}})), None);
        assert_eq!(turn_error_message(&json!({"turn": {"error": null}})), None);
    }
}
