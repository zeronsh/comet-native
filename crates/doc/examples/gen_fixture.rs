//! Generates a session-doc snapshot fixture for the cross-language compat check
//! (`edge/scripts/compat-check.mjs`). Usage: `cargo run -p comet-doc --example gen_fixture -- <out>`

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SegmentWriter, SessionCommandEntry,
    SessionCommandPayload, SessionCommandStatus, SessionDoc, SessionMessageEntry,
    fold_event_into_parts,
};
use comet_proto::{AgentEvent, ToolCall};

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: gen_fixture <out-path>");
    let doc = SessionDoc::init("chat-fixture-1").expect("init");

    // User message.
    doc.push_message(&SessionMessageEntry {
        id: "m-user-1".into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t0".into(),
            text: "Run the tests please".into(),
        }],
        created_at: 1_700_000_000_000,
        device_id: "device-rust".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    })
    .expect("push user message");

    // Streamed assistant message: text -> tool -> result -> text, exactly as the engine writes it.
    let mut writer = SegmentWriter::begin(&doc, "m-assistant-1", "device-rust", 1_700_000_001_000)
        .expect("begin");
    let mut folded = Vec::new();
    for delta in ["Sure — ", "running", " them now."] {
        folded = fold_event_into_parts(&folded, &AgentEvent::TextDelta { text: delta.into() });
        writer.sync(&folded).expect("sync");
    }
    folded = fold_event_into_parts(
        &folded,
        &AgentEvent::ToolCall {
            id: "tool-1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        },
    );
    writer.sync(&folded).expect("sync");
    folded = fold_event_into_parts(
        &folded,
        &AgentEvent::ToolResult {
            id: "tool-1".into(),
            is_error: false,
        },
    );
    folded = fold_event_into_parts(
        &folded,
        &AgentEvent::TextDelta {
            text: "All green.".into(),
        },
    );
    writer
        .finish(&folded, MessageStatus::Complete)
        .expect("finish");

    // A queued command with a host outcome.
    doc.queue_command(&SessionCommandEntry {
        id: "cmd-1".into(),
        payload: SessionCommandPayload::Steer {
            prompt: "also run clippy".into(),
            message_id: None,
        },
        issued_by: "device-rust".into(),
        issued_at: 1_700_000_002_000,
        based_on: None,
        expires_at: None,
        status: SessionCommandStatus::Pending,
        resolution: None,
    })
    .expect("queue command");
    doc.set_command_status("cmd-1", SessionCommandStatus::Applied, None)
        .expect("outcome");

    std::fs::write(&out, doc.export_snapshot().expect("export")).expect("write");
    eprintln!("wrote {out}");
}
