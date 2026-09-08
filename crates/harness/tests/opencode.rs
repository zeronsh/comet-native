//! Native opencode driver against a scripted in-test HTTP/SSE server.
//!
//! The fake speaks just enough of the v1 surface (`/global/health`,
//! `/session`, `/session/{id}/prompt_async`, `/session/{id}/abort`,
//! `/provider`, `/command`, `/global/event`) and hands the TEST full control
//! of bus timing via `emit()` — the premature-done class is exactly about
//! what happens between events, so the fixtures must own the clock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use zeron_harness::{
    CancellationToken, Harness, HarnessError, OpencodeHarness, RunControls, SteerMessage,
};
use zeron_proto::{
    AgentEvent, DoneStatus, ReasoningLevel, RunRequest, SandboxLevel, ToolCall, UserInputAnswer,
};

// ---------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeOpencode {
    base: String,
    events: broadcast::Sender<(u64, String)>,
    /// Every emitted frame, sequence-stamped — replayed to late SSE
    /// subscribers so tests may emit before the driver's stream connects.
    backlog: Arc<Mutex<Vec<(u64, String)>>>,
    /// Recorded `(path, body)` of every POST.
    posts: Arc<Mutex<Vec<(String, Value)>>>,
    providers: Arc<Mutex<Value>>,
    /// Whether an SSE subscriber existed when the FIRST prompt_async landed
    /// (the no-replay bus makes prompting before the subscription a real
    /// event-loss race — observed live on fast-failing turns).
    first_prompt_had_subscriber: Arc<Mutex<Option<bool>>>,
}

impl FakeOpencode {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (events, _) = broadcast::channel::<(u64, String)>(256);
        let fake = Self {
            base,
            events: events.clone(),
            backlog: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(Mutex::new(Vec::new())),
            providers: Arc::new(Mutex::new(json!({ "all": [], "default": {} }))),
            first_prompt_had_subscriber: Arc::new(Mutex::new(None)),
        };
        let accept = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let fake = accept.clone();
                tokio::spawn(async move { fake.serve(stream).await });
            }
        });
        fake
    }

    /// Push one bus event (the driver accepts both the bare and the
    /// `/global/event` envelope; the fake uses the enveloped form).
    fn emit(&self, payload: Value) {
        let framed = format!(
            "data: {}\n\n",
            json!({ "directory": "/", "payload": payload })
        );
        let mut backlog = self.backlog.lock().unwrap();
        let seq = backlog.len() as u64;
        backlog.push((seq, framed.clone()));
        let _ = self.events.send((seq, framed));
    }

    fn set_providers(&self, providers: Value) {
        *self.providers.lock().unwrap() = providers;
    }

    fn posts_to(&self, path: &str) -> Vec<Value> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, b)| b.clone())
            .collect()
    }

    async fn serve(self, mut stream: tokio::net::TcpStream) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        // One request per connection is enough for reqwest's default pool
        // behavior in these tests; keep-alive requests re-enter here.
        loop {
            let header_end = loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut lines = head.lines();
            let start = lines.next().unwrap_or_default().to_owned();
            let content_length = lines
                .filter_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .next()
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let body: Value = serde_json::from_slice(&buf[header_end..header_end + content_length])
                .unwrap_or(Value::Null);
            buf.drain(..header_end + content_length);

            let mut parts = start.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let path = target.split('?').next().unwrap_or_default().to_owned();

            if method == "GET" && path == "/global/event" {
                // Subscribe FIRST, then snapshot the backlog: frames landing
                // in between arrive on both channels and dedupe by sequence.
                let mut rx = self.events.subscribe();
                let replay = self.backlog.lock().unwrap().clone();
                let mut next_seq = replay.len() as u64;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          cache-control: no-cache\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                let _ = stream
                    .write_all(b"data: {\"payload\":{\"type\":\"server.connected\",\"properties\":{}}}\n\n")
                    .await;
                for (_, frame) in &replay {
                    if stream.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                }
                let _ = stream.flush().await;
                while let Ok((seq, frame)) = rx.recv().await {
                    if seq < next_seq {
                        continue;
                    }
                    next_seq = seq + 1;
                    if stream.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.flush().await;
                }
                return;
            }

            if method == "POST" {
                if path.ends_with("/prompt_async") {
                    let mut first = self.first_prompt_had_subscriber.lock().unwrap();
                    if first.is_none() {
                        *first = Some(self.events.receiver_count() > 0);
                    }
                }
                self.posts.lock().unwrap().push((path.clone(), body));
            }
            let (status, payload) = self.route(&method, &path);
            let body = payload.to_string();
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(resp.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn route(&self, method: &str, path: &str) -> (&'static str, Value) {
        match (method, path) {
            ("GET", "/global/health") => ("200 OK", json!({ "healthy": true })),
            ("GET", "/provider") => ("200 OK", self.providers.lock().unwrap().clone()),
            ("GET", "/command") => (
                "200 OK",
                json!([{ "name": "init", "description": "Create AGENTS.md" }]),
            ),
            ("POST", "/session") => ("200 OK", json!({ "id": "ses_test" })),
            ("GET", "/session/ses_resume") => ("200 OK", json!({ "id": "ses_resume" })),
            ("GET", p) if p.starts_with("/session/") => ("404 Not Found", json!({})),
            ("POST", p) if p.ends_with("/prompt_async") => ("204 No Content", json!({})),
            ("POST", p) if p.ends_with("/abort") => ("200 OK", json!(true)),
            ("POST", p) if p.contains("/permission/") || p.contains("/question/") => {
                ("200 OK", json!(true))
            }
            _ => ("404 Not Found", json!({ "missing": path })),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
        worktree: None,
    }
}

#[allow(clippy::type_complexity)]
fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steering) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: q.options.first().cloned().into_iter().collect(),
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

fn harness(fake: &FakeOpencode) -> OpencodeHarness {
    OpencodeHarness::new().with_base_url(fake.base.clone())
}

/// Emit the standard opening frames of an assistant turn.
fn assistant_message(fake: &FakeOpencode, session: &str, message: &str) {
    fake.emit(json!({
        "type": "session.status",
        "properties": { "sessionID": session, "status": { "type": "busy" } },
    }));
    fake.emit(json!({
        "type": "message.updated",
        "properties": { "info": { "id": message, "role": "assistant", "sessionID": session } },
    }));
}

fn idle(fake: &FakeOpencode, session: &str) {
    fake.emit(json!({
        "type": "session.status",
        "properties": { "sessionID": session, "status": { "type": "idle" } },
    }));
}

async fn next_event(
    stream: &mut (impl futures::Stream<Item = Result<AgentEvent, HarnessError>> + Unpin),
) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("event within budget")
        .expect("stream open")
        .expect("ok event")
}

/// Poll until `path` has received `n` POSTs; returns their bodies.
async fn wait_posts(fake: &FakeOpencode, path: &str, n: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let posts = fake.posts_to(path);
            if posts.len() >= n {
                return posts;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{path} never saw {n} posts"))
}

/// Drain until a Done arrives; returns everything seen (Done last).
async fn drain_to_done(
    stream: &mut (impl futures::Stream<Item = Result<AgentEvent, HarnessError>> + Unpin),
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(stream).await;
        let done = matches!(&ev, AgentEvent::Done { .. });
        events.push(ev);
        if done {
            return events;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thinking_streams_and_the_turn_settles_only_on_idle() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");

    let started = next_event(&mut stream).await;
    assert!(matches!(
        &started,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "ses_test"
    ));
    // AvailableCommands from /command.
    let commands = next_event(&mut stream).await;
    assert!(matches!(
        &commands,
        AgentEvent::AvailableCommands { commands } if commands.len() == 1
    ));

    assistant_message(&fake, "ses_test", "msg_1");
    // Reasoning part: open snapshot → deltas → closing snapshot (full text,
    // must dedup to nothing).
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_r", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "reasoning", "text": "",
        }},
    }));
    fake.emit(json!({
        "type": "message.part.delta",
        "properties": {
            "sessionID": "ses_test", "messageID": "msg_1", "partID": "prt_r",
            "field": "text", "delta": "let me think",
        },
    }));
    let thinking = next_event(&mut stream).await;
    assert!(matches!(
        &thinking,
        AgentEvent::ReasoningDelta { text } if text == "let me think"
    ));
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_r", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "reasoning", "text": "let me think",
        }},
    }));

    // Text streams; the turn must NOT settle during the quiet gap after it —
    // only idle ends the turn (the premature-done regression).
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_t", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "text", "text": "Hello",
        }},
    }));
    let text = next_event(&mut stream).await;
    assert!(matches!(&text, AgentEvent::TextDelta { text } if text == "Hello"));
    let quiet = tokio::time::timeout(Duration::from_millis(600), stream.next()).await;
    assert!(quiet.is_err(), "nothing may settle a quiet-but-live turn");

    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.first(),
        Some(AgentEvent::AssistantMessageCompleted { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id: Some(sid),
            ..
        }) if sid == "ses_test"
    ));
}

#[tokio::test]
async fn foreign_session_idle_never_settles_our_turn() {
    // The exact bug in opencode's own ACP layer: the first idle observed —
    // any session's — settled the turn.
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await; // SessionStarted
    let _ = next_event(&mut stream).await; // AvailableCommands

    assistant_message(&fake, "ses_test", "msg_1");
    idle(&fake, "ses_OTHER");
    let quiet = tokio::time::timeout(Duration::from_millis(600), stream.next()).await;
    assert!(quiet.is_err(), "a foreign session's idle settled our turn");

    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn model_and_advertised_variant_ride_the_prompt() {
    let fake = FakeOpencode::start().await;
    fake.set_providers(json!({
        "all": [{
            "id": "anthropic",
            "name": "Anthropic",
            "models": { "opus": { "name": "Opus", "variants": { "high": {}, "max": {} } } },
        }],
    }));
    let (controls, _steer, _token) = controls();
    let mut req = request("hi");
    req.model = Some("anthropic/opus".into());
    req.reasoning = Some(ReasoningLevel::XHigh);
    let mut stream = harness(&fake).run(req, controls).await.expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    let prompts = wait_posts(&fake, "/session/ses_test/prompt_async", 1).await;
    assert_eq!(prompts[0]["model"]["providerID"], "anthropic");
    assert_eq!(prompts[0]["model"]["modelID"], "opus");
    // XHigh isn't advertised: the ladder clamps to "high".
    assert_eq!(prompts[0]["variant"], "high");
    assert_eq!(prompts[0]["parts"][0]["text"], "hi");

    assistant_message(&fake, "ses_test", "msg_1");
    idle(&fake, "ses_test");
    drain_to_done(&mut stream).await;
}

#[tokio::test]
async fn steer_queues_mid_turn_and_delivers_at_idle() {
    let fake = FakeOpencode::start().await;
    let (controls, steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    assistant_message(&fake, "ses_test", "msg_1");
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_t", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "text", "text": "working",
        }},
    }));
    let _ = next_event(&mut stream).await; // TextDelta

    steer
        .send(SteerMessage {
            prompt: "also do this".into(),
            message_id: None,
        })
        .await
        .unwrap();
    // Give the steer time to land in the queue, then end turn 1.
    tokio::time::sleep(Duration::from_millis(100)).await;
    idle(&fake, "ses_test");

    let ev = next_event(&mut stream).await;
    assert!(
        matches!(&ev, AgentEvent::Steered { .. }),
        "queued steer must continue the run at the turn boundary, got {ev:?}"
    );
    // The steer went out as a second prompt on the SAME session.
    let prompts = wait_posts(&fake, "/session/ses_test/prompt_async", 2).await;
    assert_eq!(prompts[1]["parts"][0]["text"], "also do this");

    // Turn 2 settles normally.
    assistant_message(&fake, "ses_test", "msg_2");
    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn interrupt_aborts_and_settles_interrupted() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    assistant_message(&fake, "ses_test", "msg_1");
    token.cancel();
    wait_posts(&fake, "/session/ses_test/abort", 1).await;
    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        })
    ));
}

#[tokio::test]
async fn provider_retries_surface_and_cap_out() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    let retry = |attempt: u64| {
        json!({
            "type": "session.status",
            "properties": { "sessionID": "ses_test", "status": {
                "type": "retry", "attempt": attempt,
                "message": "AI_APICallError: unreachable", "next": 0,
            }},
        })
    };
    fake.emit(retry(1));
    fake.emit(retry(3));
    let ev = next_event(&mut stream).await;
    let AgentEvent::Error { message } = &ev else {
        panic!("expected a retry error chip, got {ev:?}");
    };
    assert!(
        message.contains("retrying") && message.contains("attempt 3"),
        "{message}"
    );
    assert!(message.contains("unreachable"), "{message}");

    fake.emit(retry(8));
    let ev = next_event(&mut stream).await;
    let AgentEvent::Error { message } = &ev else {
        panic!("expected the give-up chip, got {ev:?}");
    };
    assert!(message.contains("Giving up"), "{message}");
    // The driver aborted the turn; the server answers with idle.
    wait_posts(&fake, "/session/ses_test/abort", 1).await;
    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(_),
            ..
        })
    ));
}

#[tokio::test]
async fn session_error_with_no_content_settles_errored() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    fake.emit(json!({
        "type": "session.error",
        "properties": { "sessionID": "ses_test", "error": {
            "name": "ProviderAuthError",
            "data": { "message": "no credentials for anthropic" },
        }},
    }));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::Error { message } if message.contains("no credentials")
    ));
    // opencode re-emits the same failure with an exception-name prefix and a
    // stack — that must NOT mint a second chip (field report: every failure
    // rendered twice).
    fake.emit(json!({
        "type": "session.error",
        "properties": { "sessionID": "ses_test", "error": {
            "name": "UnknownError",
            "data": { "message": "ProviderAuthError: no credentials for anthropic\n    at stack" },
        }},
    }));
    let quiet = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
    assert!(
        quiet.is_err(),
        "duplicate error must not mint a second chip"
    );
    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(e),
            ..
        }) if e.contains("no credentials")
    ));
}

#[tokio::test]
async fn subagent_task_streams_tagged_and_settles_from_the_task_part() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("spawn"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    assistant_message(&fake, "ses_test", "msg_1");
    // The task tool part registers the chip and binds by metadata.
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_task", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "tool", "tool": "task",
            "state": {
                "status": "running",
                "input": { "description": "Viz probe", "prompt": "run", "subagent_type": "general" },
                "metadata": { "sessionId": "ses_child", "parentSessionId": "ses_test" },
            },
        }},
    }));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
            if id == "prt_task" && name == "Agent: Viz probe"
    ));

    // Child comes up and streams: prompt in, assistant text out — tagged.
    fake.emit(json!({
        "type": "session.created",
        "properties": { "info": {
            "id": "ses_child", "parentID": "ses_test",
            "title": "Viz probe (@general subagent)",
        }},
    }));
    fake.emit(json!({
        "type": "message.updated",
        "properties": { "info": { "id": "msg_cu", "role": "user", "sessionID": "ses_child" } },
    }));
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_cu", "messageID": "msg_cu", "sessionID": "ses_child",
            "type": "text", "text": "run",
        }},
    }));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::Subagent { parent_tool_use_id, event }
            if parent_tool_use_id == "prt_task"
                && matches!(&**event, AgentEvent::UserMessage { text } if text == "run")
    ));
    fake.emit(json!({
        "type": "message.updated",
        "properties": { "info": { "id": "msg_ca", "role": "assistant", "sessionID": "ses_child" } },
    }));
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_ca", "messageID": "msg_ca", "sessionID": "ses_child",
            "type": "text", "text": "finished",
        }},
    }));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::Subagent { event, .. }
            if matches!(&**event, AgentEvent::TextDelta { text } if text == "finished")
    ));

    // The task part completing settles the chip: ToolResult + tagged Done.
    fake.emit(json!({
        "type": "message.part.updated",
        "properties": { "part": {
            "id": "prt_task", "messageID": "msg_1", "sessionID": "ses_test",
            "type": "tool", "tool": "task",
            "state": {
                "status": "completed",
                "input": { "description": "Viz probe" },
                "output": "<task_result>finished</task_result>",
                "title": "Viz probe",
                "metadata": { "sessionId": "ses_child" },
                "time": { "start": 1, "end": 2 },
            },
        }},
    }));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::ToolResult { id, is_error: false, .. } if id == "prt_task"
    ));
    let ev = next_event(&mut stream).await;
    assert!(matches!(
        &ev,
        AgentEvent::Subagent { parent_tool_use_id, event }
            if parent_tool_use_id == "prt_task"
                && matches!(&**event, AgentEvent::Done { status: DoneStatus::Completed, .. })
    ));

    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn resume_reuses_the_durable_session() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut req = request("continue");
    req.resume = Some("ses_resume".into());
    let mut stream = harness(&fake).run(req, controls).await.expect("run starts");
    let started = next_event(&mut stream).await;
    assert!(matches!(
        &started,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "ses_resume"
    ));
    let _ = next_event(&mut stream).await;
    wait_posts(&fake, "/session/ses_resume/prompt_async", 1).await;

    assistant_message(&fake, "ses_resume", "msg_1");
    idle(&fake, "ses_resume");
    drain_to_done(&mut stream).await;
}

#[tokio::test]
async fn slash_command_routes_through_the_command_endpoint() {
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("/init the repo"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;

    let commands = wait_posts(&fake, "/session/ses_test/command", 1).await;
    assert_eq!(commands[0]["command"], "init");
    assert_eq!(commands[0]["arguments"], "the repo");
    assert!(fake.posts_to("/session/ses_test/prompt_async").is_empty());

    assistant_message(&fake, "ses_test", "msg_1");
    idle(&fake, "ses_test");
    drain_to_done(&mut stream).await;
}

#[tokio::test]
async fn first_prompt_waits_for_the_live_event_subscription() {
    // The v1 bus has no replay: a fast-failing turn (bad model id) emits
    // busy → session.error → idle within ~200ms of the prompt. Prompting
    // before the SSE stream exists loses the whole turn (observed live,
    // 1.18.21) — the driver must gate the first prompt on the connection.
    let fake = FakeOpencode::start().await;
    let (controls, _steer, _token) = controls();
    let mut stream = harness(&fake)
        .run(request("hi"), controls)
        .await
        .expect("run starts");
    let _ = next_event(&mut stream).await;
    let _ = next_event(&mut stream).await;
    wait_posts(&fake, "/session/ses_test/prompt_async", 1).await;
    assert_eq!(
        *fake.first_prompt_had_subscriber.lock().unwrap(),
        Some(true),
        "prompt must not be posted before the /global/event subscription exists"
    );

    // And the fast-failure lifecycle settles promptly (all three frames in
    // one burst), not via the stall watchdog.
    fake.emit(json!({
        "type": "session.status",
        "properties": { "sessionID": "ses_test", "status": { "type": "busy" } },
    }));
    fake.emit(json!({
        "type": "session.error",
        "properties": { "sessionID": "ses_test", "error": {
            "name": "UnknownError",
            "data": { "message": "Model not found: opencode/gone-model" },
        }},
    }));
    idle(&fake, "ses_test");
    let events = drain_to_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(e),
            ..
        }) if e.contains("Model not found")
    ));
}

#[tokio::test]
async fn models_discover_from_the_provider_catalog() {
    let fake = FakeOpencode::start().await;
    fake.set_providers(json!({
        "all": [
            {
                "id": "opencode",
                "name": "OpenCode Zen",
                "models": { "big-pickle": { "name": "Big Pickle" } },
            },
            {
                "id": "catalog-only",
                "name": "Needs A Key",
                "models": { "locked": { "name": "Locked" } },
            },
        ],
        "connected": ["opencode"],
    }));
    let harness = harness(&fake);
    let models = harness.models().await.expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "opencode/big-pickle");
    // Commands were primed off the same probe.
    let commands = harness.commands().await.expect("commands");
    assert_eq!(commands[0].name, "init");
}
