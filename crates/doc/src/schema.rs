//! Session doc schema over `loro` — Rust port of `packages/session-doc/src/schema.ts`.
//!
//! Container layout (MUST stay shape-compatible with the TS edge/tail materializer):
//! - `meta`:     LoroMap  { chatId: string, schemaVersion: number }         (host-only writer)
//! - `messages`: LoroList of LoroMap {
//!   id, role, parts: LoroList<part map>, createdAt, deviceId, status?, continuationOf? }
//! - `commands`: LoroList of LoroMap {
//!   id, kind, payload(json), issuedBy, issuedAt, basedOn?, expiresAt?, status, resolution? }
//! - `queue`:    LoroMovableList of LoroMap {
//!   id, text, attachments?, issuedBy, issuedAt, editedAt? }        (any device writes)
//!
//! Part maps: { id, kind: "text"|"reasoning"|"tool"|"input"|"error", text?: LoroText,
//! reasoning?: LoroText, call?: json, isError?, questions?: json, resolved?, message? }.
//! Text bodies are **LoroText** so streaming appends RLE-merge (1.03x oplog overhead vs
//! 125x for whole-value rewrites).

use loro::{ExportMode, LoroDoc, LoroError, LoroList, LoroMap, LoroText, LoroValue, ToJson};
use serde::{Deserialize, Serialize};

use crate::commands::{SessionCommandEntry, SessionCommandStatus};
use crate::constants::{SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT};
use crate::parts::{MessagePart, MessageStatus, SubagentStatus};

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("schema: {0}")]
    Schema(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// One entry in the doc's `messages` list (`SessionMessageEntry` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    /// Epoch millis.
    pub created_at: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<String>,
}

/// The doc-resident flat part map (`DocMessagePart` in TS). Distinct from the app-layer
/// [`MessagePart`]: input parts key on their request id, error parts store `message`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocPartJson {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Thinking body for `kind: "reasoning"` (additive). Deliberately NOT the
    /// `text` field: old readers' unknown-kind fallback renders `text` as
    /// prose, so reasoning riding `text` would leak raw thinking into old
    /// transcripts; on this field they degrade to an invisible empty part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    questions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Tool output summary (additive — absent on old rows and old writers;
    /// pre-strip writers stored up to 4KB of capped output here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    /// Capped inline tool diff (additive; pre-strip writers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff: Option<serde_json::Value>,
    /// Sidecar key of the full output (additive, docs/chat2-sync.md A1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_ref: Option<String>,
    /// Full-output byte length (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    /// Sidecar key of the full diff JSON (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff_ref: Option<String>,
    /// Per-file diff stats (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff_stats: Option<serde_json::Value>,
    /// Subagent doc/blob ref carried by a spawn chip (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_ref: Option<String>,
    /// Subagent lifecycle ("running"/"done"/"failed", additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_status: Option<String>,
    /// One-line live tail of the subagent's output (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_tail: Option<String>,
}

/// App parts → doc part json (mirror of `toDocParts`).
fn to_doc_part(part: &MessagePart) -> Result<DocPartJson, DocError> {
    Ok(match part {
        MessagePart::Text { id, text } => DocPartJson {
            id: id.clone(),
            kind: "text".into(),
            text: Some(text.clone()),
            ..Default::default()
        },
        MessagePart::Reasoning { id, text } => DocPartJson {
            id: id.clone(),
            kind: "reasoning".into(),
            reasoning: Some(text.clone()),
            ..Default::default()
        },
        MessagePart::Tool {
            id,
            call,
            is_error,
            resolved,
            output,
            diff,
            output_ref,
            output_bytes,
            diff_ref,
            diff_stats,
            subagent_ref,
            subagent_status,
            subagent_tail,
        } => DocPartJson {
            id: id.clone(),
            kind: "tool".into(),
            call: Some(serde_json::to_value(call)?),
            // TS shape parity: `isError` is written only once the tool result arrived;
            // its presence IS the resolution marker.
            is_error: if *resolved { Some(*is_error) } else { None },
            output: output.clone(),
            diff: diff.as_ref().map(serde_json::to_value).transpose()?,
            output_ref: output_ref.clone(),
            output_bytes: *output_bytes,
            diff_ref: diff_ref.clone(),
            diff_stats: diff_stats.as_ref().map(serde_json::to_value).transpose()?,
            subagent_ref: subagent_ref.clone(),
            subagent_status: subagent_status.map(|s| {
                match s {
                    SubagentStatus::Running => "running",
                    SubagentStatus::Done => "done",
                    SubagentStatus::Failed => "failed",
                }
                .to_owned()
            }),
            subagent_tail: subagent_tail.clone(),
            ..Default::default()
        },
        MessagePart::Input {
            id: _,
            request_id,
            questions,
            resolved,
        } => DocPartJson {
            id: request_id.clone(),
            kind: "input".into(),
            questions: Some(serde_json::to_value(questions)?),
            resolved: Some(*resolved),
            ..Default::default()
        },
        MessagePart::Error { id, message } => DocPartJson {
            id: id.clone(),
            kind: "error".into(),
            message: Some(message.clone()),
            ..Default::default()
        },
    })
}

/// Doc part json → app part (mirror of `fromDocParts`; malformed degrades to empty text).
fn from_doc_part(p: DocPartJson) -> MessagePart {
    match p.kind.as_str() {
        "tool" => match p.call.and_then(|c| serde_json::from_value(c).ok()) {
            Some(call) => MessagePart::Tool {
                id: p.id,
                call,
                is_error: p.is_error.unwrap_or(false),
                resolved: p.is_error.is_some(),
                output: p.output,
                diff: p.diff.and_then(|d| serde_json::from_value(d).ok()),
                output_ref: p.output_ref,
                output_bytes: p.output_bytes,
                diff_ref: p.diff_ref,
                diff_stats: p.diff_stats.and_then(|s| serde_json::from_value(s).ok()),
                subagent_ref: p.subagent_ref,
                subagent_status: p.subagent_status.as_deref().and_then(|s| match s {
                    "running" => Some(SubagentStatus::Running),
                    "done" => Some(SubagentStatus::Done),
                    "failed" => Some(SubagentStatus::Failed),
                    _ => None,
                }),
                subagent_tail: p.subagent_tail,
            },
            None => MessagePart::Text {
                id: p.id,
                text: String::new(),
            },
        },
        "input" => MessagePart::Input {
            id: p.id.clone(),
            request_id: p.id,
            questions: p
                .questions
                .and_then(|q| serde_json::from_value(q).ok())
                .unwrap_or_default(),
            resolved: p.resolved.unwrap_or(false),
        },
        "error" => MessagePart::Error {
            id: p.id,
            message: p.message.unwrap_or_default(),
        },
        "reasoning" => MessagePart::Reasoning {
            id: p.id,
            text: p.reasoning.unwrap_or_default(),
        },
        _ => MessagePart::Text {
            id: p.id,
            text: p.text.unwrap_or_default(),
        },
    }
}

/// A session doc handle: typed access over a LoroDoc with the schema above.
pub struct SessionDoc {
    doc: LoroDoc,
}

impl SessionDoc {
    /// Wrap an existing doc (e.g. imported from a snapshot).
    pub fn from_doc(doc: LoroDoc) -> Self {
        Self { doc }
    }

    /// Create + initialize a fresh doc for `chat_id` (host-only).
    pub fn init(chat_id: &str) -> Result<Self, DocError> {
        let doc = LoroDoc::new();
        let meta = doc.get_map("meta");
        meta.insert("chatId", chat_id)?;
        meta.insert("schemaVersion", SESSION_SCHEMA_VERSION as i64)?;
        doc.commit();
        Ok(Self { doc })
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    /// A single atomic value prevents tokens and capacity from tearing on sync.
    pub fn context_usage(&self) -> Option<zeron_proto::ContextUsage> {
        let loro::ValueOrContainer::Value(LoroValue::String(value)) =
            self.doc.get_map("meta").get("contextUsage")?
        else {
            return None;
        };
        serde_json::from_str(&value).ok()
    }

    pub fn update_context_usage(
        &self,
        tokens: Option<u64>,
        window: Option<u64>,
    ) -> Result<(), DocError> {
        let previous = self.context_usage().unwrap_or_default();
        let next = zeron_proto::ContextUsage {
            tokens: tokens.or(previous.tokens),
            window: window.filter(|n| *n > 0).or(previous.window),
        };
        if next != previous {
            self.doc
                .get_map("meta")
                .insert("contextUsage", serde_json::to_string(&next)?)?;
            self.doc.commit();
        }
        Ok(())
    }

    pub fn clear_context_usage(&self) -> Result<(), DocError> {
        self.doc.get_map("meta").delete("contextUsage")?;
        self.doc.commit();
        Ok(())
    }

    pub fn chat_id(&self) -> Option<String> {
        match self.doc.get_map("meta").get("chatId") {
            Some(loro::ValueOrContainer::Value(LoroValue::String(s))) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Insert a complete message entry (user/system messages, command-side inserts).
    /// Streaming assistant entries go through [`SegmentWriter`].
    pub fn push_message(&self, entry: &SessionMessageEntry) -> Result<(), DocError> {
        let messages = self.doc.get_list("messages");
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(&map, entry)?;
        let parts = map.insert_container("parts", LoroList::new())?;
        for part in &entry.parts {
            push_part(&parts, part)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// Read all entries (continuations NOT joined — see `join_continuation_entries`).
    ///
    /// Malformed entries are SKIPPED, not fatal: a torn intermediate state
    /// (an entry map imported before the update that fills its fields) or a
    /// peer on a newer schema must degrade to a missing row, never blank the
    /// whole transcript — one bad entry took down every publish for the chat
    /// (2026-07-31, "missing field `id`" during a multi-update import).
    pub fn read_entries(&self) -> Result<Vec<SessionMessageEntry>, DocError> {
        // Materialize only the messages container — a whole-doc deep value
        // here also serialized the commands ledger on every 120ms commit tick.
        let messages = self
            .doc
            .get_list("messages")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(messages)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match entry_from_json(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(
                        chat = %self.chat_id().unwrap_or_default(),
                        error = %err,
                        "skipping unsalvageable transcript entry"
                    );
                    None
                }
            })
            .collect())
    }

    /// Read the commands ledger.
    ///
    /// Same skip-not-fail policy as `read_entries`: any device can append
    /// here, and one malformed entry must not wedge command draining for the
    /// chat forever (an unparseable command can't be executed anyway).
    pub fn read_commands(&self) -> Result<Vec<SessionCommandEntry>, DocError> {
        // Container-scoped for the same reason as `read_entries`: the drain
        // loop runs this per tick and must not pay for the transcript.
        let commands = self
            .doc
            .get_list("commands")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(commands)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match serde_json::from_value(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed command entry");
                    None
                }
            })
            .collect())
    }

    /// Append a command entry (rule 1: own entries only, append-only).
    pub fn queue_command(&self, entry: &SessionCommandEntry) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        let map = commands.push_container(LoroMap::new())?;
        map.insert("id", entry.id.as_str())?;
        map.insert(
            "kind",
            serde_json::to_value(entry.kind())?
                .as_str()
                .ok_or_else(|| DocError::Schema("kind not a string".into()))?,
        )?;
        map.insert(
            "payload",
            loro_value_from_json(&serde_json::to_value(&entry.payload)?),
        )?;
        map.insert("issuedBy", entry.issued_by.as_str())?;
        map.insert("issuedAt", entry.issued_at)?;
        if let Some(based_on) = &entry.based_on {
            map.insert(
                "basedOn",
                loro_value_from_json(&serde_json::to_value(based_on)?),
            )?;
        }
        if let Some(expires_at) = entry.expires_at {
            map.insert("expiresAt", expires_at)?;
        }
        map.insert(
            "status",
            serde_json::to_value(entry.status)?
                .as_str()
                .ok_or_else(|| DocError::Schema("status not a string".into()))?,
        )?;
        self.doc.commit();
        Ok(())
    }

    /// Rule 2: host (or the issuing composer, for `cancelled`) writes an outcome.
    pub fn set_command_status(
        &self,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        for i in 0..commands.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                commands.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == command_id
                );
                if id_matches {
                    map.insert(
                        "status",
                        serde_json::to_value(status)?
                            .as_str()
                            .ok_or_else(|| DocError::Schema("status not a string".into()))?,
                    )?;
                    if let Some(r) = resolution {
                        map.insert("resolution", r)?;
                    }
                    self.doc.commit();
                    return Ok(());
                }
            }
        }
        Err(DocError::Schema(format!("command {command_id} not found")))
    }

    /// Stamp a terminal status on an existing message entry by id (recovery:
    /// abandoned `streaming` entries from a dead run are stamped `aborted`).
    /// Returns `false` when no entry with that id exists.
    pub fn set_message_status(
        &self,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
                );
                if id_matches {
                    map.insert("status", status_str(status))?;
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Append an error part to an existing entry (crash recovery: the aborted
    /// entry must SAY why it ended — "Run interrupted by engine restart…" —
    /// not just truncate silently). Returns `false` when no entry matches.
    pub fn append_error_part(
        &self,
        message_id: &str,
        part_id: &str,
        message: &str,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let id_matches = matches!(
                entry.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
            );
            if !id_matches {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            // Idempotent per part id (recovery may re-run on a crash loop).
            for j in 0..parts.len() {
                if let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                    && matches!(
                        part.get("id"),
                        Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == part_id
                    )
                {
                    return Ok(true);
                }
            }
            push_part(
                &parts,
                &MessagePart::Error {
                    id: part_id.to_string(),
                    message: message.to_string(),
                },
            )?;
            self.doc.commit();
            return Ok(true);
        }
        Ok(false)
    }

    /// Mark the input part carrying `request_id` resolved, wherever it lives
    /// (input parts store the request id as their part id). The live-run path
    /// resolves through the entry fold; this direct write is for answers to a
    /// question whose run already died — no fold owns the entry anymore.
    /// Returns `false` when no such part exists.
    pub fn resolve_input(&self, request_id: &str) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                let is_input = matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "input"
                );
                let id_matches = matches!(
                    part.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == request_id
                );
                if is_input && id_matches {
                    part.insert("resolved", true)?;
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Update a subagent SPAWN CHIP (a tool part) in place, wherever it
    /// lives: `resolved`-style stamping for the eager-done world, where the
    /// chip's entry is usually already finished by the time the background
    /// subagent produces its lifecycle. Searched from the NEWEST entry back
    /// (the chip belongs to a recent turn). `None` fields are left as-is.
    pub fn update_subagent_chip(
        &self,
        part_id: &str,
        subagent_ref: Option<&str>,
        status: Option<&str>,
        tail: Option<&str>,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in (0..messages.len()).rev() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                let is_tool = matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "tool"
                );
                let id_matches = matches!(
                    part.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == part_id
                );
                if is_tool && id_matches {
                    // Genus gate: subagent lifecycle only ever lands on a
                    // SPAWN call. Mis-keyed tagged traffic (a driver bug —
                    // claude's background shells settled through the
                    // subagent subtype, 2026-08-20) must not decorate an
                    // ordinary chip with a ref to a doc it never had.
                    let is_spawn = part
                        .get("call")
                        .and_then(|v| match v {
                            loro::ValueOrContainer::Value(v) => serde_json::to_value(v).ok(),
                            _ => None,
                        })
                        .and_then(|j| serde_json::from_value::<zeron_proto::ToolCall>(j).ok())
                        .is_some_and(|c| c.is_subagent_spawn());
                    if !is_spawn {
                        return Ok(false);
                    }
                    if let Some(r) = subagent_ref {
                        part.insert("subagentRef", r)?;
                    }
                    if let Some(s) = status {
                        part.insert("subagentStatus", s)?;
                    }
                    if let Some(t) = tail {
                        part.insert("subagentTail", t)?;
                    }
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Export a snapshot (persistence) — `ExportMode::Snapshot`.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }
}

fn write_entry_scalar_fields(map: &LoroMap, entry: &SessionMessageEntry) -> Result<(), DocError> {
    map.insert("id", entry.id.as_str())?;
    map.insert(
        "role",
        match entry.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        },
    )?;
    map.insert("createdAt", entry.created_at)?;
    map.insert("deviceId", entry.device_id.as_str())?;
    if let Some(status) = entry.status {
        map.insert("status", status_str(status))?;
    }
    if let Some(continuation_of) = &entry.continuation_of {
        map.insert("continuationOf", continuation_of.as_str())?;
    }
    Ok(())
}

fn status_str(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Streaming => "streaming",
        MessageStatus::Complete => "complete",
        MessageStatus::Aborted => "aborted",
    }
}

/// Append one part map to a parts list; text bodies become LoroText containers.
fn push_part(parts: &LoroList, part: &MessagePart) -> Result<(), DocError> {
    let map = parts.push_container(LoroMap::new())?;
    let doc_part = to_doc_part(part)?;
    map.insert("id", doc_part.id.as_str())?;
    map.insert("kind", doc_part.kind.as_str())?;
    if let Some(text) = &doc_part.text {
        let t = map.insert_container("text", LoroText::new())?;
        t.insert(0, text)?;
    }
    if let Some(reasoning) = &doc_part.reasoning {
        // LoroText like `text`: reasoning streams token by token, and
        // whole-value rewrites cost ~125x the oplog of a text append.
        let t = map.insert_container("reasoning", LoroText::new())?;
        t.insert(0, reasoning)?;
    }
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(output) = &doc_part.output {
        map.insert("output", output.as_str())?;
    }
    if let Some(diff) = &doc_part.diff {
        map.insert("diff", loro_value_from_json(diff))?;
    }
    if let Some(output_ref) = &doc_part.output_ref {
        map.insert("outputRef", output_ref.as_str())?;
    }
    if let Some(output_bytes) = doc_part.output_bytes {
        map.insert("outputBytes", output_bytes as i64)?;
    }
    if let Some(diff_ref) = &doc_part.diff_ref {
        map.insert("diffRef", diff_ref.as_str())?;
    }
    if let Some(diff_stats) = &doc_part.diff_stats {
        map.insert("diffStats", loro_value_from_json(diff_stats))?;
    }
    if let Some(subagent_ref) = &doc_part.subagent_ref {
        map.insert("subagentRef", subagent_ref.as_str())?;
    }
    if let Some(subagent_status) = &doc_part.subagent_status {
        map.insert("subagentStatus", subagent_status.as_str())?;
    }
    if let Some(subagent_tail) = &doc_part.subagent_tail {
        map.insert("subagentTail", subagent_tail.as_str())?;
    }
    Ok(())
}

fn entry_from_json(v: serde_json::Value) -> Result<SessionMessageEntry, DocError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawEntry {
        id: String,
        role: MessageRole,
        #[serde(default)]
        parts: Vec<DocPartJson>,
        created_at: i64,
        device_id: String,
        #[serde(default)]
        status: Option<MessageStatus>,
        #[serde(default)]
        continuation_of: Option<String>,
    }
    match serde_json::from_value::<RawEntry>(v.clone()) {
        Ok(raw) => Ok(SessionMessageEntry {
            id: raw.id,
            role: raw.role,
            parts: raw.parts.into_iter().map(from_doc_part).collect(),
            created_at: raw.created_at,
            device_id: raw.device_id,
            status: raw.status,
            continuation_of: raw.continuation_of,
        }),
        // 2026-08-10 incident rule: a missing field must cost AT MOST what
        // the field carried — never the entry, never the transcript. Rooms
        // merge writes from every device and app version; one bad writer
        // (or one mangled export) blanking whole sessions for every reader
        // is exactly what tonight looked like.
        Err(strict_err) => salvage_entry(v, strict_err),
    }
}

/// Field-level salvage for entries the strict shape rejects. Missing
/// identity/attribution fields get deterministic stand-ins (content-hashed
/// id, so repeated reads and continuation joins stay stable); parts are
/// salvaged individually — a part missing `kind` is inferred from its
/// content shape, and only truly contentless parts are dropped.
fn salvage_entry(
    v: serde_json::Value,
    strict_err: serde_json::Error,
) -> Result<SessionMessageEntry, DocError> {
    let Some(obj) = v.as_object() else {
        return Err(DocError::Json(strict_err));
    };
    let stable_hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        v.to_string().hash(&mut hasher);
        hasher.finish()
    };
    let str_field = |key: &str| obj.get(key).and_then(|x| x.as_str()).map(str::to_owned);
    let id = str_field("id").unwrap_or_else(|| format!("recovered-{stable_hash:016x}"));
    let role = obj
        .get("role")
        .and_then(|r| serde_json::from_value::<MessageRole>(r.clone()).ok())
        .unwrap_or(MessageRole::Assistant);
    let mut parts = Vec::new();
    let mut dropped_parts = 0usize;
    if let Some(raw_parts) = obj.get("parts").and_then(|p| p.as_array()) {
        for (ix, part) in raw_parts.iter().enumerate() {
            match serde_json::from_value::<DocPartJson>(part.clone()) {
                Ok(p) => parts.push(from_doc_part(p)),
                Err(_) => match salvage_part(part, &id, ix) {
                    Some(p) => parts.push(p),
                    None => dropped_parts += 1,
                },
            }
        }
    }
    tracing::warn!(
        entry = %id,
        error = %strict_err,
        salvaged_parts = parts.len(),
        dropped_parts,
        "transcript entry failed strict parse; salvaged"
    );
    Ok(SessionMessageEntry {
        id,
        role,
        parts,
        created_at: obj.get("createdAt").and_then(|x| x.as_i64()).unwrap_or(0),
        device_id: str_field("deviceId").unwrap_or_default(),
        status: obj
            .get("status")
            .and_then(|s| serde_json::from_value(s.clone()).ok()),
        continuation_of: str_field("continuationOf"),
    })
}

/// Salvage one part whose strict `DocPartJson` parse failed: infer the kind
/// from the content shape (`text` → text part, parseable `call` → tool
/// part). `None` only when nothing renderable survives.
fn salvage_part(part: &serde_json::Value, entry_id: &str, ix: usize) -> Option<MessagePart> {
    let obj = part.as_object()?;
    let id = obj
        .get("id")
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{entry_id}#recovered-{ix}"));
    if let Some(reasoning) = obj.get("reasoning").and_then(|x| x.as_str()) {
        return Some(MessagePart::Reasoning {
            id,
            text: reasoning.to_owned(),
        });
    }
    if let Some(text) = obj.get("text").and_then(|x| x.as_str()) {
        return Some(MessagePart::Text {
            id,
            text: text.to_owned(),
        });
    }
    if let Some(call) = obj
        .get("call")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
    {
        return Some(MessagePart::Tool {
            id,
            call,
            is_error: obj
                .get("isError")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            resolved: obj
                .get("resolved")
                .and_then(|x| x.as_bool())
                .unwrap_or(true),
            output: obj
                .get("output")
                .and_then(|x| x.as_str())
                .map(str::to_owned),
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        });
    }
    if let Some(message) = obj.get("message").and_then(|x| x.as_str()) {
        return Some(MessagePart::Error {
            id,
            message: message.to_owned(),
        });
    }
    None
}

/// Render-time continuation join at the entry level (`joinContinuations` in TS):
/// concatenate continuation entries' parts onto their root, in list order.
pub fn join_continuation_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    if !entries.iter().any(|e| e.continuation_of.is_some()) {
        return entries;
    }
    let mut out: Vec<SessionMessageEntry> = Vec::with_capacity(entries.len());
    let mut root_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in entries {
        match &entry.continuation_of {
            Some(root_id) => {
                if let Some(&at) = root_index.get(root_id) {
                    out[at].parts.extend(entry.parts);
                } else {
                    // Orphan continuation — surface as its own entry rather than dropping.
                    out.push(entry);
                }
            }
            None => {
                root_index.insert(entry.id.clone(), out.len());
                out.push(entry);
            }
        }
    }
    out
}

/// Incremental streaming writer for one assistant entry.
///
/// Port of zeron's `DocSegmentWriter` diff discipline: called with the *folded* parts of the
/// live segment (from `fold_event_into_parts`) at each commit tick, it diffs against what's in
/// the doc and writes only the delta:
/// - trailing text growth → `LoroText` append (RLE-merged),
/// - new parts → pushed,
/// - tool call refresh / resolution / input resolution → in-place map updates.
///
/// Invariant relied upon: the fold only ever APPENDS parts or grows the trailing text; earlier
/// text never mutates. Tool/input parts may update fields in place.
pub struct SegmentWriter<'a> {
    doc: &'a SessionDoc,
    /// Index of this entry in the `messages` list.
    entry_index: usize,
    /// Mirror of what we've written so far (part id → app part).
    written: Vec<MessagePart>,
}

impl<'a> SegmentWriter<'a> {
    /// Begin a streaming assistant entry: pushes the entry with `status: streaming`, no parts.
    pub fn begin(
        doc: &'a SessionDoc,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<Self, DocError> {
        let messages = doc.doc.get_list("messages");
        let entry_index = messages.len();
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(
            &map,
            &SessionMessageEntry {
                id: entry_id.into(),
                role: MessageRole::Assistant,
                parts: vec![],
                created_at,
                device_id: device_id.into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            },
        )?;
        map.insert_container("parts", LoroList::new())?;
        doc.doc.commit();
        Ok(Self {
            doc,
            entry_index,
            written: Vec::new(),
        })
    }

    /// Reattach to a streaming entry a prior [`Self::begin`] pushed on the
    /// same doc, with the caller-held mirror of what was already written —
    /// the seam that lets a sink hold `(entry_index, written)` between
    /// coalesced flushes instead of a doc-borrowing writer.
    pub fn resume(doc: &'a SessionDoc, entry_index: usize, written: Vec<MessagePart>) -> Self {
        Self {
            doc,
            entry_index,
            written,
        }
    }

    /// The state a later [`Self::resume`] needs.
    pub fn into_state(self) -> (usize, Vec<MessagePart>) {
        (self.entry_index, self.written)
    }

    fn entry_map(&self) -> Result<LoroMap, DocError> {
        let messages = self.doc.doc.get_list("messages");
        match messages.get(self.entry_index) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
            _ => Err(DocError::Schema("streaming entry map missing".into())),
        }
    }

    fn parts_list(&self) -> Result<LoroList, DocError> {
        match self.entry_map()?.get("parts") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => Ok(list),
            _ => Err(DocError::Schema(
                "streaming entry parts list missing".into(),
            )),
        }
    }

    /// Diff `folded` (the full folded segment so far) into the doc.
    pub fn sync(&mut self, folded: &[MessagePart]) -> Result<(), DocError> {
        let parts = self.parts_list()?;
        let mut dirty = false;

        for (i, part) in folded.iter().enumerate() {
            match self.written.get(i) {
                None => {
                    push_part(&parts, part)?;
                    self.written.push(part.clone());
                    dirty = true;
                }
                Some(prev) if prev == part => {}
                Some(prev) => {
                    // Trailing growth of a text-bodied part appends into its
                    // LoroText container instead of rewriting the map value.
                    let grown = match (prev, part) {
                        (
                            MessagePart::Text { text: old, .. },
                            MessagePart::Text { text: new, .. },
                        ) if new.starts_with(old.as_str()) => Some(("text", old, new)),
                        (
                            MessagePart::Reasoning { text: old, .. },
                            MessagePart::Reasoning { text: new, .. },
                        ) if new.starts_with(old.as_str()) => Some(("reasoning", old, new)),
                        _ => None,
                    };
                    match grown {
                        Some((field, old, new)) => {
                            let delta = &new[old.len()..];
                            if !delta.is_empty() {
                                let part_map = part_map_at(&parts, i)?;
                                match part_map.get(field) {
                                    Some(loro::ValueOrContainer::Container(
                                        loro::Container::Text(t),
                                    )) => {
                                        let len = t.len_unicode();
                                        t.insert(len, delta)?;
                                    }
                                    _ => {
                                        return Err(DocError::Schema(format!(
                                            "{field} part missing LoroText"
                                        )));
                                    }
                                }
                                dirty = true;
                            }
                        }
                        None => {
                            // Field-level update (tool refresh/resolve, input resolve, or a
                            // non-append text rewrite, which the fold shouldn't produce —
                            // rewrite the part map fields defensively).
                            let part_map = part_map_at(&parts, i)?;
                            update_part_fields(&part_map, part)?;
                            dirty = true;
                        }
                    }
                    self.written[i] = part.clone();
                }
            }
        }

        if dirty {
            self.doc.doc.commit();
        }
        Ok(())
    }

    /// Finish the stream: sync final parts and stamp a terminal status.
    pub fn finish(mut self, folded: &[MessagePart], status: MessageStatus) -> Result<(), DocError> {
        self.sync(folded)?;
        let map = self.entry_map()?;
        map.insert("status", status_str(status))?;
        self.doc.doc.commit();
        Ok(())
    }
}

fn part_map_at(parts: &LoroList, index: usize) -> Result<LoroMap, DocError> {
    match parts.get(index) {
        Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
        _ => Err(DocError::Schema(format!("part map missing at {index}"))),
    }
}

/// In-place field refresh for tool/input parts (and defensive text rewrite).
fn update_part_fields(map: &LoroMap, part: &MessagePart) -> Result<(), DocError> {
    let doc_part = to_doc_part(part)?;
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(output) = &doc_part.output {
        map.insert("output", output.as_str())?;
    }
    if let Some(diff) = &doc_part.diff {
        map.insert("diff", loro_value_from_json(diff))?;
    }
    if let Some(output_ref) = &doc_part.output_ref {
        map.insert("outputRef", output_ref.as_str())?;
    }
    if let Some(output_bytes) = doc_part.output_bytes {
        map.insert("outputBytes", output_bytes as i64)?;
    }
    if let Some(diff_ref) = &doc_part.diff_ref {
        map.insert("diffRef", diff_ref.as_str())?;
    }
    if let Some(diff_stats) = &doc_part.diff_stats {
        map.insert("diffStats", loro_value_from_json(diff_stats))?;
    }
    if let Some(subagent_ref) = &doc_part.subagent_ref {
        map.insert("subagentRef", subagent_ref.as_str())?;
    }
    if let Some(subagent_status) = &doc_part.subagent_status {
        map.insert("subagentStatus", subagent_status.as_str())?;
    }
    if let Some(subagent_tail) = &doc_part.subagent_tail {
        map.insert("subagentTail", subagent_tail.as_str())?;
    }
    if let Some(text) = &doc_part.text {
        // Defensive path only — the fold never rewrites earlier text.
        if let Some(loro::ValueOrContainer::Container(loro::Container::Text(t))) = map.get("text") {
            t.update(text, Default::default())
                .map_err(|e| DocError::Schema(e.to_string()))?;
        }
    }
    if let Some(reasoning) = &doc_part.reasoning {
        // Same defensive rewrite for the reasoning body.
        if let Some(loro::ValueOrContainer::Container(loro::Container::Text(t))) =
            map.get("reasoning")
        {
            t.update(reasoning, Default::default())
                .map_err(|e| DocError::Schema(e.to_string()))?;
        }
    }
    Ok(())
}

pub(crate) fn loro_value_from_json(v: &serde_json::Value) -> LoroValue {
    LoroValue::from(v.clone())
}

/// Tail sidecar shape (`SessionTail` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTail {
    pub chat_id: String,
    pub schema_version: u32,
    pub messages: Vec<SessionMessageEntry>,
    pub total_messages: usize,
    pub updated_at: i64,
}

/// Materialize the last-N joined messages (`materializeTail` in TS).
pub fn materialize_tail(
    doc: &SessionDoc,
    now: i64,
    tail_count: usize,
) -> Result<SessionTail, DocError> {
    let all = join_continuation_entries(doc.read_entries()?);
    let total = all.len();
    let start = total.saturating_sub(if tail_count == 0 {
        TAIL_MESSAGE_COUNT
    } else {
        tail_count
    });
    Ok(SessionTail {
        chat_id: doc.chat_id().unwrap_or_default(),
        schema_version: SESSION_SCHEMA_VERSION,
        messages: all[start..].to_vec(),
        total_messages: total,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::fold_event_into_parts;
    use zeron_proto::{AgentEvent, ToolCall};

    fn user_entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn round_trips_message_entries() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");
        assert_eq!(
            entries[0].parts,
            vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into()
            }]
        );
        assert_eq!(doc.chat_id().as_deref(), Some("chat-1"));
    }

    #[test]
    fn segment_sync_persists_subagent_chip_fields_on_live_parts() {
        // The eager-done world's OTHER path: the chip mutates while its
        // segment still streams (codex fan-outs) — update_part_fields must
        // carry the chip fields or SegmentWriter::sync drops them silently.
        let doc = SessionDoc::init("c1").unwrap();
        let mut w = SegmentWriter::begin(&doc, "e1", "dev", 1).unwrap();
        let mut part = MessagePart::Tool {
            id: "call_alpha".into(),
            call: zeron_proto::ToolCall::Unknown {
                name: "Agent: alpha".into(),
                input: None,
            },
            is_error: false,
            resolved: false,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        };
        w.sync(std::slice::from_ref(&part)).unwrap();
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            subagent_tail,
            ..
        } = &mut part
        {
            *subagent_ref = Some("c1--sub--call_alpha".into());
            *subagent_status = Some(SubagentStatus::Running);
            *subagent_tail = Some("scanning".into());
        }
        w.sync(std::slice::from_ref(&part)).unwrap();
        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool {
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } => {
                assert_eq!(subagent_ref.as_deref(), Some("c1--sub--call_alpha"));
                assert_eq!(subagent_status, &Some(SubagentStatus::Running));
                assert_eq!(subagent_tail.as_deref(), Some("scanning"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn update_subagent_chip_refuses_non_spawn_parts() {
        // The genus gate at the doc boundary: whatever id a driver keys its
        // tagged traffic to, subagent lifecycle only ever lands on a SPAWN
        // call (claude's background shells settled through the subagent
        // subtype and turned Run chips into dead spawn links, 2026-08-20).
        let doc = SessionDoc::init("c1").unwrap();
        let mut w = SegmentWriter::begin(&doc, "e1", "dev", 1).unwrap();
        let tool = |id: &str, call: zeron_proto::ToolCall| MessagePart::Tool {
            id: id.into(),
            call,
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        };
        let parts = vec![
            tool(
                "toolu_bash",
                zeron_proto::ToolCall::Exec {
                    command: "git clone …".into(),
                },
            ),
            tool(
                "toolu_spawn",
                zeron_proto::ToolCall::Unknown {
                    name: "Agent: scan".into(),
                    input: None,
                },
            ),
        ];
        w.sync(&parts).unwrap();
        assert!(
            doc.update_subagent_chip(
                "toolu_spawn",
                Some("c1--sub--toolu_spawn"),
                Some("running"),
                None
            )
            .unwrap()
        );
        assert!(
            !doc.update_subagent_chip(
                "toolu_bash",
                Some("c1--sub--toolu_bash"),
                Some("done"),
                None
            )
            .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool {
                subagent_ref,
                subagent_status,
                ..
            } => {
                assert!(subagent_ref.is_none());
                assert!(subagent_status.is_none());
            }
            other => panic!("{other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Tool { subagent_ref, .. } => {
                assert_eq!(subagent_ref.as_deref(), Some("c1--sub--toolu_spawn"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn resolve_input_stamps_the_part_in_place() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: "r1".into(),
                request_id: "r1".into(),
                questions: vec![],
                resolved: false,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            // The orphan case: the run died and recovery stamped the entry.
            status: Some(MessageStatus::Aborted),
            continuation_of: None,
        })
        .unwrap();
        assert!(!doc.resolve_input("nope").unwrap());
        assert!(doc.resolve_input("r1").unwrap());
        let entries = doc.read_entries().unwrap();
        assert!(matches!(
            &entries[0].parts[0],
            MessagePart::Input { resolved: true, .. }
        ));
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let bytes = doc.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = SessionDoc::from_doc(other);
        assert_eq!(
            restored.read_entries().unwrap(),
            doc.read_entries().unwrap()
        );
    }

    #[test]
    fn two_peers_converge_on_concurrent_inserts() {
        let a = SessionDoc::init("chat-1").unwrap();
        let b = SessionDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        a.push_message(&user_entry("m-a", "from a")).unwrap();
        b.push_message(&user_entry("m-b", "from b")).unwrap();

        // Cross-import updates.
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();

        let ea = a.read_entries().unwrap();
        let eb = b.read_entries().unwrap();
        assert_eq!(ea, eb);
        assert_eq!(ea.len(), 2); // one insert from each peer, converged in the same order
    }

    #[test]
    fn segment_writer_streams_text_incrementally() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "Hel".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "lo".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
            },
        );
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Some(MessageStatus::Complete));
        assert_eq!(entries[0].parts.len(), 2);
        match &entries[0].parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("unexpected {other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Tool {
                resolved, is_error, ..
            } => {
                assert!(*resolved);
                assert!(!*is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Reasoning streams like text: LoroText appends into the `reasoning`
    /// field (not `text` — old readers' unknown-kind fallback renders `text`
    /// as prose), interleaving with text/tool parts, and round-trips through
    /// the doc read path.
    #[test]
    fn segment_writer_streams_reasoning_incrementally() {
        let doc = SessionDoc::init("chat-r").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ReasoningDelta {
                text: "let me ".into(),
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ReasoningDelta {
                text: "think".into(),
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "Done".into() });
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].parts.len(), 2);
        match &entries[0].parts[0] {
            MessagePart::Reasoning { id, text } => {
                assert_eq!(id, "r0");
                assert_eq!(text, "let me think");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Text { text, .. } => assert_eq!(text, "Done"),
            other => panic!("unexpected {other:?}"),
        }

        // Old-reader degradation contract: the raw doc part carries NO
        // `text` field, so a pre-reasoning client's fallback renders an
        // empty (invisible) text part, never the thinking body.
        let value = doc.doc.get_deep_value().to_json_value();
        let part = &value["messages"][0]["parts"][0];
        assert_eq!(part["kind"], "reasoning");
        assert_eq!(part["reasoning"], "let me think");
        assert!(part.get("text").is_none(), "{part:?}");
    }

    /// The ToolResult resolution path goes through `update_part_fields` —
    /// the stripped output summary, sidecar refs, and diff stats must survive
    /// the doc round trip (regression: output/diff were silently dropped
    /// there while `to_doc_part` carried them).
    #[test]
    fn segment_writer_round_trips_stripped_tool_fields() {
        let doc = SessionDoc::init("chat-2").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: false,
                output: Some("total 0\nmore lines".into()),
                diff: Some(zeron_proto::ToolDiff {
                    path: "/w/a.rs".into(),
                    old_text: Some("old\n".into()),
                    new_text: "new\n".into(),
                }),
            },
        );
        crate::parts::apply_sidecar_refs("chat-2", &mut folded);
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool {
                output,
                output_ref,
                output_bytes,
                diff,
                diff_ref,
                diff_stats,
                ..
            } => {
                // One-liner chips: the fold drops outputs entirely (journal
                // only), so even a direct apply_sidecar_refs call has no
                // output to key — diff stats still get their ref (this test
                // calls apply_sidecar_refs directly; the live fold no longer
                // does).
                assert_eq!(output.as_deref(), None);
                assert_eq!(output_ref.as_deref(), None);
                assert_eq!(*output_bytes, None);
                assert!(diff.is_none(), "no inline diff text in the doc");
                assert_eq!(diff_ref.as_deref(), Some("chat-2/t1.diff"));
                let stats = diff_stats.as_ref().expect("stats survive");
                assert_eq!(stats[0].path, "/w/a.rs");
                assert_eq!((stats[0].additions, stats[0].deletions), (1, 1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Old pre-strip docs carry inline `output`/`diff` — they must still read
    /// back (schema changes are serde-additive ONLY; old readers, old docs).
    #[test]
    fn pre_strip_doc_parts_still_round_trip() {
        let doc = SessionDoc::init("chat-3").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
                output: Some("full inline output\nline 2".into()),
                diff: Some(zeron_proto::ToolDiff {
                    path: "/w/a.rs".into(),
                    old_text: Some("old".into()),
                    new_text: "new".into(),
                }),
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool { output, diff, .. } => {
                assert_eq!(output.as_deref(), Some("full inline output\nline 2"));
                assert_eq!(diff.as_ref().unwrap().new_text, "new");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn set_message_status_stamps_existing_entry() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("m1", "hello");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        doc.push_message(&entry).unwrap();

        assert!(
            doc.set_message_status("m1", MessageStatus::Aborted)
                .unwrap()
        );
        assert!(
            !doc.set_message_status("nope", MessageStatus::Aborted)
                .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].status, Some(MessageStatus::Aborted));
    }

    #[test]
    fn command_queue_and_outcome_round_trip() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        doc.queue_command(&entry).unwrap();
        doc.set_command_status("c1", SessionCommandStatus::Applied, None)
            .unwrap();
        let commands = doc.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, SessionCommandStatus::Applied);
        assert_eq!(commands[0].payload, entry.payload);
    }

    #[test]
    fn tail_materializes_last_n_joined() {
        let doc = SessionDoc::init("chat-1").unwrap();
        for i in 0..5 {
            doc.push_message(&user_entry(&format!("m{i}"), &format!("msg {i}")))
                .unwrap();
        }
        let tail = materialize_tail(&doc, 99, 2).unwrap();
        assert_eq!(tail.total_messages, 5);
        assert_eq!(tail.messages.len(), 2);
        assert_eq!(tail.messages[1].id, "m4");
        assert_eq!(tail.chat_id, "chat-1");
    }

    /// 2026-08-10 incident: entries/parts missing strict fields must salvage
    /// field-by-field — a fresh reader importing a room's merged doc must
    /// never render a BLANK transcript because some writer (old app version,
    /// other-platform client, mangled export) omitted metadata.
    #[test]
    fn malformed_entries_salvage_instead_of_vanishing() {
        // Entry missing `id` + `deviceId`; one part missing `kind` but
        // carrying text; one part contentless (dropped).
        let v = serde_json::json!({
            "role": "assistant",
            "createdAt": 123,
            "parts": [
                { "id": "p1", "text": "still readable" },
                { "opaque": true },
                { "id": "p3", "kind": "text", "text": "well-formed" }
            ]
        });
        let entry = entry_from_json(v.clone()).expect("salvaged");
        assert!(
            entry.id.starts_with("recovered-"),
            "deterministic stand-in id"
        );
        let again = entry_from_json(v).expect("salvaged again");
        assert_eq!(entry.id, again.id, "recovered id is stable across reads");
        assert_eq!(entry.role, MessageRole::Assistant);
        assert_eq!(entry.created_at, 123);
        assert_eq!(
            entry.parts.len(),
            2,
            "text parts survive, contentless part dropped"
        );
        match &entry.parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "still readable"),
            other => panic!("unexpected {other:?}"),
        }

        // Tool part missing `kind` but with a parseable call salvages as Tool.
        let v = serde_json::json!({
            "role": "assistant",
            "createdAt": 1,
            "parts": [ { "id": "t1", "call": { "kind": "exec", "command": "ls" }, "output": "x" } ]
        });
        let entry = entry_from_json(v).expect("salvaged");
        assert!(matches!(
            &entry.parts[0],
            MessagePart::Tool { resolved: true, .. }
        ));

        // Only non-objects are truly unsalvageable.
        assert!(entry_from_json(serde_json::json!("garbage")).is_err());
        assert!(entry_from_json(serde_json::json!(42)).is_err());

        // Well-formed entries take the strict path unchanged.
        let v = serde_json::json!({
            "id": "m1", "role": "user", "createdAt": 5, "deviceId": "d",
            "parts": [ { "id": "p", "kind": "text", "text": "hi" } ]
        });
        let entry = entry_from_json(v).expect("strict");
        assert_eq!(entry.id, "m1");
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::*;
    #[test]
    fn context_snapshot_survives_remote_import_restart_and_rebuild() {
        let host = SessionDoc::init("context-chat").unwrap();
        assert_eq!(host.context_usage(), None);
        host.update_context_usage(Some(150_000), Some(200_000))
            .unwrap();
        let replica = SessionDoc::from_doc(LoroDoc::new());
        replica
            .doc()
            .import(&host.export_snapshot().unwrap())
            .unwrap();
        assert_eq!(replica.context_usage(), host.context_usage());
        let version = host.doc().oplog_vv();
        // Compaction is a replacement, not an accumulating counter. Zero capacity is invalid.
        host.update_context_usage(Some(0), Some(0)).unwrap();
        replica
            .doc()
            .import(&host.doc().export(ExportMode::updates(&version)).unwrap())
            .unwrap();
        assert_eq!(
            replica.context_usage(),
            Some(zeron_proto::ContextUsage {
                tokens: Some(0),
                window: Some(200_000)
            })
        );
        host.update_context_usage(None, Some(1_000_000)).unwrap();
        assert_eq!(host.context_usage().unwrap().tokens, Some(0));
        let rebuilt = crate::rebuild_thin_doc(&host).unwrap().doc;
        assert_eq!(rebuilt.context_usage(), host.context_usage());
        rebuilt.clear_context_usage().unwrap();
        assert_eq!(rebuilt.context_usage(), None);
    }
}
