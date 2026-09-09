use super::*;

/// Real HTTP/SSE transport with explicitly ordered turn events. No provider or
/// installed CLI is involved, so duplicate completion frames are reproducible.
struct TurnWire {
    bus: mpsc::UnboundedSender<Value>,
    requests: mpsc::UnboundedReceiver<String>,
    events: mpsc::Receiver<Result<AgentEvent, HarnessError>>,
    interrupt: tokio_util::sync::CancellationToken,
    server: tokio::task::JoinHandle<()>,
    run: tokio::task::JoinHandle<()>,
}

impl Drop for TurnWire {
    fn drop(&mut self) {
        self.interrupt.cancel();
        self.run.abort();
        self.server.abort();
    }
}

impl TurnWire {
    async fn start(queued: bool) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (bus, bus_rx) = mpsc::unbounded_channel::<Value>();
        let bus_rx = Arc::new(tokio::sync::Mutex::new(Some(bus_rx)));
        let (request_tx, requests) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let bus_rx = bus_rx.clone();
                let request_tx = request_tx.clone();
                connections.spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 4096];
                    let header_end = loop {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 { return; }
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let header = String::from_utf8_lossy(&request[..header_end]);
                    let path = header.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_owned();
                    let length = header.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    while request.len() < header_end + length {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 { return; }
                        request.extend_from_slice(&buf[..n]);
                    }
                    if path == "/global/event" {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: connected\n\n").await.unwrap();
                        let mut events = bus_rx.lock().await.take().unwrap();
                        while let Some(event) = events.recv().await {
                            if socket.write_all(format!("data: {event}\n\n").as_bytes()).await.is_err() { break; }
                        }
                        return;
                    }
                    let body = match path.as_str() {
                        "/session" => r#"{"id":"fixture"}"#,
                        "/command" => "[]",
                        _ => "{}",
                    };
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    if path.ends_with("/prompt_async") || path.ends_with("/abort") {
                        let _ = request_tx.send(path);
                    }
                });
            }
        });
        let (event_tx, events) = mpsc::channel(64);
        let (steer_tx, steering) = mpsc::channel(4);
        if queued {
            steer_tx
                .send(crate::SteerMessage {
                    prompt: "second".into(),
                    message_id: None,
                })
                .await
                .unwrap();
        }
        drop(steer_tx);
        let interrupt = tokio_util::sync::CancellationToken::new();
        let run = tokio::spawn(run_session(Session {
            server: Server::attached(base),
            event_tx,
            controls: RunControls {
                request_input: Box::new(|_| panic!("fixture must not ask for input")),
                steering,
                interrupt: interrupt.clone(),
            },
            request: serde_json::from_value(
                json!({"prompt":"first", "cwd":"", "sandbox":"workspace-write"}),
            )
            .unwrap(),
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_millis(50),
            known_commands: Some(vec![]),
        }));
        Self {
            bus,
            requests,
            events,
            interrupt,
            server,
            run,
        }
    }

    async fn request(&mut self, suffix: &str) {
        let path = tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(path.ends_with(suffix), "unexpected request: {path}");
    }

    fn status(&self, status: &str) {
        self.bus.send(json!({"type":"session.status", "properties":{"sessionID":"fixture", "status":{"type":status}}})).unwrap();
    }

    fn idle(&self) {
        self.bus
            .send(json!({"type":"session.idle", "properties":{"sessionID":"fixture"}}))
            .unwrap();
    }

    async fn done(&mut self) -> (DoneStatus, String) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut text = String::new();
            loop {
                match self.events.recv().await.unwrap().unwrap() {
                    AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                    AgentEvent::Done { status, .. } => return (status, text),
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn queued_turn_ignores_previous_turn_duplicate_idle() {
    for status_first in [true, false] {
        let mut wire = TurnWire::start(true).await;
        wire.request("/prompt_async").await;
        wire.status("busy");
        // Both completion encodings belong to the first turn. The first frame
        // submits the queued prompt; the second must not finish that new turn.
        if status_first {
            wire.status("idle");
            wire.idle();
        } else {
            wire.idle();
            wire.status("idle");
        }
        wire.request("/prompt_async").await;
        wire.status("busy");
        wire.bus.send(json!({"type":"message.updated", "properties":{"info":{"id":"answer", "sessionID":"fixture", "role":"assistant"}}})).unwrap();
        wire.bus.send(json!({"type":"message.part.updated", "properties":{"part":{"id":"text", "messageID":"answer", "sessionID":"fixture", "type":"text", "text":"SECOND_OK"}}})).unwrap();
        wire.status("idle");
        let (status, text) = wire.done().await;
        assert_eq!(status, DoneStatus::Completed);
        assert_eq!(
            text, "SECOND_OK",
            "queued turn was completed before its response"
        );
    }
}

#[tokio::test]
async fn prompt_error_and_interrupt_before_busy_still_settle() {
    let mut wire = TurnWire::start(false).await;
    wire.request("/prompt_async").await;
    wire.bus.send(json!({"type":"session.error", "properties":{"sessionID":"fixture", "error":{"name":"ProviderError", "data":{"message":"bad model"}}}})).unwrap();
    wire.idle();
    assert_eq!(wire.done().await.0, DoneStatus::Errored);

    let mut wire = TurnWire::start(false).await;
    wire.request("/prompt_async").await;
    wire.interrupt.cancel();
    wire.request("/abort").await;
    wire.idle();
    assert_eq!(wire.done().await.0, DoneStatus::Interrupted);
}

#[tokio::test]
async fn catalog_decodes_fragmented_http_without_retaining_unused_fields() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = json!({
        "all": [{"id":"test", "models":{"model":{"name":"模型", "variants":{"high":{"unused":"x".repeat(128 * 1024)}}}}}],
        "connected":["test"]
    }).to_string();
    let expected: ProviderCatalog = serde_json::from_str(&body).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.read(&mut [0; 4096]).await.unwrap();
        socket
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
            )
            .await
            .unwrap();
        for chunk in body.as_bytes().chunks(257) {
            socket.write_all(chunk).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let response = reqwest::get(format!("http://{address}")).await.unwrap();
    let catalog: ProviderCatalog = decode_json_response(response).await.unwrap();
    assert_eq!(
        models_from_providers(&catalog),
        models_from_providers(&expected)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_catalog_decode_releases_a_stalled_http_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.read(&mut [0; 4096]).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n{")
            .await
            .unwrap();
        let result = socket.read(&mut [0; 1]).await;
        let _ = closed_tx.send(result);
    });
    let response = reqwest::get(format!("http://{address}")).await.unwrap();
    let decode = tokio::spawn(decode_json_response::<ProviderCatalog>(response));
    tokio::time::sleep(Duration::from_millis(20)).await;
    decode.abort();
    assert!(decode.await.unwrap_err().is_cancelled());
    let result = tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "cancel must close the body reader"
    );
    server.await.unwrap();
}
use serde_json::json;

#[test]
fn provider_discovery_ignores_metadata_and_accepts_null_optional_fields() {
    let catalog: ProviderCatalog = serde_json::from_str(
        r#"{
        "all": [{
            "id": "local", "name": null,
            "models": {
                "small": {"name": null, "variants": null},
                "thinking": {
                    "variants": {"high": {"nested": [{"unused": "configuration"}]}},
                    "capabilities": {"large": [1, 2, 3]},
                    "cost": {"input": 1}, "limit": {"context": 200000}
                }
            },
            "options": {"unused": [true, false, null]}
        }, {"id": "empty", "models": null}],
        "connected": null,
        "default": {"unused": "model"}
    }"#,
    )
    .unwrap();
    let models = models_from_providers(&catalog);
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "local/small");
    assert_eq!(models[0].description.as_deref(), Some("local"));
    assert!(models[0].reasoning_levels.is_empty());
    assert_eq!(models[1].reasoning_levels, vec![ReasoningLevel::High]);
    assert_eq!(
        pick_variant(&catalog, "local", "thinking", Some(ReasoningLevel::High)).as_deref(),
        Some("high")
    );
}

#[test]
fn models_map_provider_catalog_with_variant_ladders() {
    let providers: ProviderCatalog = serde_json::from_value(json!({
        "all": [
            {
                "id": "anthropic",
                "name": "Anthropic",
                "models": {
                    "claude-opus-5": {
                        "name": "Claude Opus 5",
                        "variants": {"low": {}, "medium": {}, "high": {}, "max": {}},
                    },
                    "claude-haiku-4-5": {"name": "Claude Haiku 4.5"},
                }
            },
            {
                "id": "opencode",
                "name": "OpenCode Zen",
                "models": {"big-pickle": {"name": "Big Pickle"}}
            }
        ],
        "default": {},
        "connected": ["anthropic"],
    }))
    .unwrap();
    let models = models_from_providers(&providers);
    // `connected` filters: the full catalog is 194 providers / 7k models of
    // which the user can run almost none (v0.2.21 field report).
    assert_eq!(models.len(), 2);
    let opus = models
        .iter()
        .find(|m| m.id == "anthropic/claude-opus-5")
        .expect("opus");
    assert_eq!(opus.label, "Claude Opus 5");
    assert_eq!(opus.description.as_deref(), Some("Anthropic"));
    assert_eq!(
        opus.reasoning_levels,
        vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max
        ]
    );
    let haiku = models
        .iter()
        .find(|m| m.id == "anthropic/claude-haiku-4-5")
        .expect("haiku");
    assert!(haiku.reasoning_levels.is_empty());
    assert!(
        !models.iter().any(|m| m.id == "opencode/big-pickle"),
        "unconnected providers stay out of the picker"
    );
}

#[test]
fn missing_connected_list_falls_back_to_the_full_catalog() {
    let providers: ProviderCatalog = serde_json::from_value(json!({
        "all": [
            {"id": "a", "models": {"m1": {}}},
            {"id": "b", "models": {"m2": {}}},
        ],
    }))
    .unwrap();
    assert_eq!(models_from_providers(&providers).len(), 2);
    let providers: ProviderCatalog = serde_json::from_value(json!({
        "all": [
            {"id": "a", "models": {"m1": {}}},
            {"id": "b", "models": {"m2": {}}},
        ],
        "connected": [],
    }))
    .unwrap();
    assert_eq!(models_from_providers(&providers).len(), 2);
}

#[test]
fn variants_only_ride_models_that_advertise_them() {
    let providers: ProviderCatalog = serde_json::from_value(json!({
        "all": [{
            "id": "anthropic",
            "models": {
                "opus": {"variants": {"high": {}, "max": {}}},
                "haiku": {},
            }
        }]
    }))
    .unwrap();
    assert_eq!(
        pick_variant(&providers, "anthropic", "opus", Some(ReasoningLevel::High)).as_deref(),
        Some("high")
    );
    // XHigh clamps down the candidate ladder to an advertised id.
    assert_eq!(
        pick_variant(&providers, "anthropic", "opus", Some(ReasoningLevel::XHigh)).as_deref(),
        Some("high")
    );
    assert_eq!(
        pick_variant(&providers, "anthropic", "haiku", Some(ReasoningLevel::High)),
        None
    );
    assert_eq!(pick_variant(&providers, "anthropic", "opus", None), None);
    assert_eq!(
        pick_variant(&providers, "missing", "opus", Some(ReasoningLevel::Low)),
        None
    );
}

#[test]
fn prompt_body_carries_model_variant_and_attachments() {
    let body = prompt_body(
        "hello",
        &Some(("anthropic".into(), "claude-opus-5".into())),
        Some("high"),
        &["/tmp/shot.png".to_owned()],
    );
    assert_eq!(body["model"]["providerID"], "anthropic");
    assert_eq!(body["model"]["modelID"], "claude-opus-5");
    assert_eq!(body["variant"], "high");
    assert_eq!(body["parts"][0]["type"], "text");
    assert_eq!(body["parts"][0]["text"], "hello");
    assert_eq!(body["parts"][1]["type"], "file");
    assert_eq!(body["parts"][1]["mime"], "image/png");
    assert_eq!(body["parts"][1]["url"], "file:///tmp/shot.png");
}

fn feed_with_assistant(message: &str) -> SessionFeed {
    let mut feed = SessionFeed::default();
    feed.assistant_messages.insert(message.into(), true);
    feed
}

#[test]
fn reasoning_parts_stream_as_reasoning_deltas() {
    let mut feed = feed_with_assistant("msg_a");
    // Opening snapshot: empty reasoning part fixes the kind.
    let open = json!({
        "id": "prt_r", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "reasoning", "text": "",
    });
    assert!(part_snapshot_events(&mut feed, &open, true, None).is_empty());
    // Deltas append as ReasoningDelta, not text.
    let props = json!({"sessionID": "ses_1", "messageID": "msg_a", "partID": "prt_r"});
    let events = part_delta_events(&mut feed, &props, "prt_r", "thinking hard");
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ReasoningDelta { text }] if text == "thinking hard"
    ));
    // The closing full snapshot re-sends everything: dedup emits nothing.
    let close = json!({
        "id": "prt_r", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "reasoning", "text": "thinking hard",
    });
    assert!(part_snapshot_events(&mut feed, &close, true, None).is_empty());
    // A longer snapshot emits only the suffix.
    let more = json!({
        "id": "prt_r", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "reasoning", "text": "thinking hard about it",
    });
    let events = part_snapshot_events(&mut feed, &more, true, None);
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ReasoningDelta { text }] if text == " about it"
    ));
}

#[test]
fn reasoning_ahead_of_its_message_role_is_held_and_replayed() {
    let mut feed = SessionFeed::default();
    let part = json!({
        "id": "prt_r", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "reasoning", "text": "early thought",
    });
    assert!(part_snapshot_events(&mut feed, &part, true, None).is_empty());
    assert_eq!(feed.pending_parts.len(), 1);
    // The role lands; replay drains the held part.
    feed.assistant_messages.insert("msg_a".into(), true);
    let mut turn = TurnState::begin(None);
    let events = replay_pending(&mut feed, "msg_a", true, &mut turn);
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ReasoningDelta { text }] if text == "early thought"
    ));
    assert!(turn.saw_content);
}

#[test]
fn main_feed_user_text_is_the_prompt_echo_and_never_renders() {
    let mut feed = SessionFeed::default();
    feed.assistant_messages.insert("msg_u".into(), false);
    let part = json!({
        "id": "prt_u", "messageID": "msg_u", "sessionID": "ses_1",
        "type": "text", "text": "the prompt",
    });
    assert!(part_snapshot_events(&mut feed, &part, true, None).is_empty());
    // On a CHILD feed the same shape is the message INTO the child.
    let mut child = SessionFeed::default();
    child.assistant_messages.insert("msg_u".into(), false);
    let events = part_snapshot_events(&mut child, &part, false, None);
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::UserMessage { text }] if text == "the prompt"
    ));
    // Re-delivered snapshots don't double the entry.
    assert!(part_snapshot_events(&mut child, &part, false, None).is_empty());
}

#[test]
fn tool_parts_open_and_resolve_once() {
    let mut feed = feed_with_assistant("msg_a");
    let running = json!({
        "id": "prt_t", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "tool", "tool": "bash", "callID": "call-1",
        "state": {"status": "running", "input": {"command": "echo ok"}},
    });
    let events = part_snapshot_events(&mut feed, &running, true, None);
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ToolCall { id, call: ToolCall::Exec { command } }]
            if id == "call-1" && command == "echo ok"
    ));
    assert!(part_snapshot_events(&mut feed, &running, true, None).is_empty());
    let done = json!({
        "id": "prt_t", "messageID": "msg_a", "sessionID": "ses_1",
        "type": "tool", "tool": "bash", "callID": "call-1",
        "state": {"status": "completed", "input": {"command": "echo ok"}, "output": "ok\n"},
    });
    let events = part_snapshot_events(&mut feed, &done, true, None);
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ToolResult { id, is_error: false, output: Some(o), .. }]
            if id == "call-1" && o == "ok\n"
    ));
}

#[test]
fn task_spawn_registers_child_by_metadata_and_completion_settles() {
    let mut feed = feed_with_assistant("msg_a");
    let mut children = HashMap::new();
    let mut pending = VecDeque::new();
    let mut unbound = HashMap::new();
    let running = json!({
        "id": "prt_task", "messageID": "msg_a", "sessionID": "ses_parent",
        "type": "tool", "tool": "task",
        "state": {
            "status": "running",
            "input": {"description": "Scan crates", "prompt": "scan", "subagent_type": "general"},
            "metadata": {"sessionId": "ses_child", "parentSessionId": "ses_parent"},
        },
    });
    let events = part_snapshot_events(
        &mut feed,
        &running,
        true,
        Some((&mut children, &mut pending, &mut unbound)),
    );
    // Genus-gated spawn naming, keyed by the PART id.
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }]
            if id == "prt_task" && name == "Agent: Scan crates"
    ));
    let child = children.get("ses_child").expect("bound child");
    assert_eq!(child.parent_tool_use_id, "prt_task");

    let completed = json!({
        "id": "prt_task", "messageID": "msg_a", "sessionID": "ses_parent",
        "type": "tool", "tool": "task",
        "state": {
            "status": "completed",
            "input": {"description": "Scan crates"},
            "output": "<task_result>done</task_result>",
            "metadata": {"sessionId": "ses_child"},
        },
    });
    assert_eq!(
        task_completion(&completed),
        Some(("ses_child".to_owned(), false))
    );
}

#[test]
fn child_binding_falls_back_to_title_match() {
    let mut children = HashMap::new();
    let mut pending = VecDeque::new();
    pending.push_back(PendingSpawn {
        tool_part_id: "prt_1".into(),
        description: "Scan crates".into(),
    });
    pending.push_back(PendingSpawn {
        tool_part_id: "prt_2".into(),
        description: "Write docs".into(),
    });
    assert!(bind_child(
        &mut children,
        &mut pending,
        "ses_b",
        "Write docs (@general subagent)"
    ));
    assert_eq!(children.get("ses_b").unwrap().parent_tool_use_id, "prt_2");
    assert_eq!(pending.len(), 1);
    // Unmatched title binds FIFO.
    assert!(bind_child(&mut children, &mut pending, "ses_a", "mystery"));
    assert_eq!(children.get("ses_a").unwrap().parent_tool_use_id, "prt_1");
    // Nothing pending: no bind.
    assert!(!bind_child(
        &mut children,
        &mut pending,
        "ses_c",
        "anything"
    ));
}

#[test]
fn questions_map_to_input_panel_shape() {
    let props = json!({
        "id": "que_1",
        "sessionID": "ses_1",
        "questions": [{
            "question": "Which color?",
            "header": "Color",
            "options": [
                {"label": "Red", "description": "warm"},
                {"label": "Blue", "description": "cool"},
            ],
            "multiple": true,
        }],
    });
    let questions = map_questions(&props);
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, "q0");
    assert_eq!(questions[0].header, "Color");
    assert_eq!(questions[0].question, "Which color?");
    assert_eq!(questions[0].options, vec!["Red", "Blue"]);
    assert!(questions[0].multi_select);
}

#[test]
fn tool_names_type_the_common_calls() {
    let call = oc_tool_call("bash", &json!({"command": "ls -la"}));
    assert_eq!(
        call,
        ToolCall::Exec {
            command: "ls -la".into()
        }
    );
    let call = oc_tool_call(
        "edit",
        &json!({"filePath": "/w/a.rs", "oldString": "a", "newString": "b"}),
    );
    assert_eq!(
        call,
        ToolCall::EditFile {
            path: "/w/a.rs".into(),
            old_string: Some("a".into()),
            new_string: Some("b".into()),
        }
    );
    let call = oc_tool_call("task", &json!({"description": "Scan crates"}));
    assert!(matches!(&call, ToolCall::Unknown { name, .. } if name == "Agent: Scan crates"));
    assert!(call.is_subagent_spawn());
    let call = oc_tool_call(
        "todowrite",
        &json!({"todos": [
            {"content": "step one", "status": "completed"},
            {"content": "step two", "status": "pending"},
        ]}),
    );
    assert!(matches!(
        &call,
        ToolCall::Todo { items } if items.len() == 2 && items[0].done && !items[1].done
    ));
    let call = oc_tool_call("mystery", &json!({"x": 1}));
    assert!(matches!(&call, ToolCall::Unknown { name, input: Some(_) } if name == "mystery"));
    assert!(!call.is_subagent_spawn());
}

#[test]
fn commands_map_from_wire() {
    let wire = json!([
        {"name": "init", "description": "Create AGENTS.md"},
        {"name": "share"},
        {"description": "nameless is dropped"},
    ]);
    let commands = commands_from_wire(&wire);
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "init");
    assert_eq!(commands[0].description, "Create AGENTS.md");
    assert_eq!(commands[1].name, "share");
}

#[test]
fn stall_env_and_startup_env_parse() {
    // Defaults (no env in test runner): bounded stall, 300s startup.
    assert_eq!(stall_bound(), Some(DEFAULT_STALL_BOUND));
    assert_eq!(startup_timeout(), DEFAULT_STARTUP_TIMEOUT);
}

#[test]
fn directory_header_percent_encodes() {
    assert_eq!(
        encode_directory("/home/u/my project"),
        "/home/u/my%20project"
    );
    assert_eq!(encode_directory("/plain/path"), "/plain/path");
}
