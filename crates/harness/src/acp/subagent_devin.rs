//! Devin subagent visualization over its ACP extension.
//!
//! Advertising `clientCapabilities._meta["cognition.ai/subagentSupport"]`
//! unlocks three tags on ordinary `session/update` frames (verified against
//! Devin CLI 3000.6.14): the parent's `run_subagent` tool call is followed by
//! `subagent_started`; child traffic carries `subagent_context.parentAgentId`;
//! and `subagent_completed` settles it. The stable child agent id is correlated
//! back to the parent's spawn tool-call id so nested traffic never leaks into
//! the parent transcript.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;
use zeron_proto::{AgentEvent, DoneStatus};

use super::normalize::map_update;

#[derive(Debug)]
struct PendingSpawn {
    tool_call_id: String,
    title: String,
}

#[derive(Default)]
pub(crate) struct DevinTracker {
    pending: VecDeque<PendingSpawn>,
    /// Devin child agent id -> the parent's run_subagent tool-call id.
    bound: HashMap<String, String>,
    /// Child ids that emitted assistant text; completion summaries are
    /// fallback text only and must not duplicate a streamed final answer.
    saw_text: HashSet<String>,
    /// Completion can race the final child frames; never reopen a settled
    /// transcript under the child id after its parent binding is gone.
    settled: HashSet<String>,
}

impl DevinTracker {
    pub(crate) fn map(&mut self, update: &Value) -> Vec<AgentEvent> {
        if let Some(started) = meta(update, "cognition.ai/subagent_started") {
            let Some(agent_id) = nonempty(started, "agentId") else {
                return Vec::new();
            };
            let title = nonempty(started, "title").unwrap_or_default();
            let index = self
                .pending
                .iter()
                .position(|p| !title.is_empty() && p.title == title)
                .or((!self.pending.is_empty()).then_some(0));
            let parent = index
                .and_then(|i| self.pending.remove(i))
                .map(|p| p.tool_call_id)
                // A missing replayed spawn degrades to a stable synthetic
                // owner instead of dropping the child transcript.
                .unwrap_or_else(|| agent_id.to_owned());
            self.bound.insert(agent_id.to_owned(), parent);
            // This lifecycle update's toolCallId is the child agent id, not a
            // real parent-chat tool call, so it must not render by itself.
            return Vec::new();
        }

        if let Some(completed) = meta(update, "cognition.ai/subagent_completed") {
            let Some(agent_id) = nonempty(completed, "agentId") else {
                return Vec::new();
            };
            if !self.settled.insert(agent_id.to_owned()) {
                return Vec::new();
            }
            let parent = self
                .bound
                .get(agent_id)
                .cloned()
                .unwrap_or_else(|| agent_id.to_owned());
            let mut events = Vec::new();
            if !self.saw_text.remove(agent_id)
                && let Some(summary) = nonempty(completed, "summary")
            {
                events.push(tag(
                    &parent,
                    AgentEvent::TextDelta {
                        text: summary.to_owned(),
                    },
                ));
            }
            let status = if completed.get("success").and_then(Value::as_bool) == Some(false) {
                DoneStatus::Errored
            } else {
                DoneStatus::Completed
            };
            events.push(tag(
                &parent,
                AgentEvent::Done {
                    status,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ));
            return events;
        }

        if is_spawn(update) {
            self.remember_spawn(update);
        }

        let child_id = meta(update, "cognition.ai/subagent_context")
            .and_then(|ctx| nonempty(ctx, "parentAgentId"));
        let Some(child_id) = child_id else {
            return map_update(update);
        };
        if self.settled.contains(child_id) {
            return Vec::new();
        }
        let parent = self
            .bound
            .get(child_id)
            .cloned()
            .unwrap_or_else(|| child_id.to_owned());
        let events = map_update(update);
        if events
            .iter()
            .any(|event| matches!(event, AgentEvent::TextDelta { .. }))
        {
            self.saw_text.insert(child_id.to_owned());
        }
        events
            .into_iter()
            .map(|event| tag(&parent, event))
            .collect()
    }

    /// Settle children when their owning ACP process/run goes away before a
    /// lifecycle completion arrives (interrupt, crash, or consumer close).
    pub(crate) fn finish_open(&mut self, status: DoneStatus) -> Vec<AgentEvent> {
        let open: Vec<(String, String)> = self
            .bound
            .iter()
            .filter(|(agent_id, _)| !self.settled.contains(*agent_id))
            .map(|(agent_id, parent)| (agent_id.clone(), parent.clone()))
            .collect();
        open.into_iter()
            .map(|(agent_id, parent)| {
                self.settled.insert(agent_id);
                tag(
                    &parent,
                    AgentEvent::Done {
                        status,
                        result: None,
                        error: None,
                        session_id: None,
                    },
                )
            })
            .collect()
    }

    fn remember_spawn(&mut self, update: &Value) {
        let id = nonempty(update, "toolCallId").unwrap_or_default();
        if id.is_empty() {
            return;
        }
        if matches!(
            update.get("status").and_then(Value::as_str),
            Some("failed" | "cancelled")
        ) {
            self.pending.retain(|p| p.tool_call_id != id);
            return;
        }
        let title = update
            .get("rawInput")
            .and_then(|raw| nonempty(raw, "title"))
            .unwrap_or_default();
        match self.pending.iter_mut().find(|p| p.tool_call_id == id) {
            Some(p) if p.title.is_empty() => p.title = title.to_owned(),
            Some(_) => {}
            None => self.pending.push_back(PendingSpawn {
                tool_call_id: id.to_owned(),
                title: title.to_owned(),
            }),
        }
    }
}

fn is_spawn(update: &Value) -> bool {
    update
        .get("_meta")
        .and_then(|m| m.get("cognition.ai/inferenceToolName"))
        .and_then(Value::as_str)
        == Some("run_subagent")
}

fn meta<'a>(update: &'a Value, key: &str) -> Option<&'a Value> {
    update.get("_meta")?.get(key)
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str().filter(|s| !s.is_empty())
}

fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zeron_proto::{AgentEvent, DoneStatus, ToolCall};

    use super::DevinTracker;

    #[test]
    fn correlates_spawn_child_traffic_and_completion() {
        let mut tracker = DevinTracker::default();
        let events = tracker.map(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "spawn-1",
            "kind": "other",
            "status": "in_progress",
            "rawInput": { "title": "Inspect auth", "task": "Review auth.rs" },
            "_meta": { "cognition.ai/inferenceToolName": "run_subagent" }
        }));
        assert!(matches!(
            &events[0],
            AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
                if id == "spawn-1" && name == "Agent: Inspect auth"
        ));

        assert!(
            tracker
                .map(&json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "agent-7",
                    "_meta": { "cognition.ai/subagent_started": {
                        "agentId": "agent-7", "title": "Inspect auth", "profile": "Explore"
                    }}
                }))
                .is_empty()
        );

        let child = tracker.map(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "Found the issue" },
            "_meta": { "cognition.ai/subagent_context": { "parentAgentId": "agent-7" }}
        }));
        assert_eq!(
            child,
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "spawn-1".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "Found the issue".into()
                }),
            }]
        );

        let completed = tracker.map(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "agent-7",
            "_meta": { "cognition.ai/subagent_completed": {
                "agentId": "agent-7", "success": true, "summary": "Found the issue"
            }}
        }));
        assert_eq!(completed.len(), 1, "streamed summary must not duplicate");
        assert!(matches!(
            &completed[0],
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "spawn-1"
                    && matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Completed, .. })
        ));
    }

    #[test]
    fn completion_summary_is_a_fallback_when_child_was_quiet() {
        let mut tracker = DevinTracker::default();
        tracker.map(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "spawn-2",
            "rawInput": { "title": "Quiet" },
            "_meta": { "cognition.ai/inferenceToolName": "run_subagent" }
        }));
        tracker.map(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "agent-8",
            "_meta": { "cognition.ai/subagent_started": {
                "agentId": "agent-8", "title": "Quiet"
            }}
        }));
        let events = tracker.map(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "agent-8",
            "_meta": { "cognition.ai/subagent_completed": {
                "agentId": "agent-8", "success": false, "summary": "Could not inspect"
            }}
        }));
        assert!(matches!(
            &events[0],
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "spawn-2"
                    && matches!(event.as_ref(), AgentEvent::TextDelta { text } if text == "Could not inspect")
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::Subagent { event, .. }
                if matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Errored, .. })
        ));
    }

    #[test]
    fn unfinished_children_settle_when_the_parent_run_ends() {
        let mut tracker = DevinTracker::default();
        tracker.map(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "spawn-3",
            "rawInput": { "title": "Background" },
            "_meta": { "cognition.ai/inferenceToolName": "run_subagent" }
        }));
        tracker.map(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "agent-9",
            "_meta": { "cognition.ai/subagent_started": {
                "agentId": "agent-9", "title": "Background"
            }}
        }));
        let events = tracker.finish_open(DoneStatus::Interrupted);
        assert!(matches!(
            &events[0],
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "spawn-3"
                    && matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Interrupted, .. })
        ));
        assert!(tracker.finish_open(DoneStatus::Interrupted).is_empty());
    }
}
