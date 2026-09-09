//! Turn-quiesce watchdog + parked self-continuation, mocked against the
//! 2026-08-12 stuck-Working incident workflow (chat 15c8be78, Claude via
//! claude-agent-acp — but the failure shape is harness-agnostic):
//!
//! 1. A turn completes (Done) → the session parks Idle.
//! 2. The agent re-invokes ITSELF on a background-task notification and
//!    streams real output with no prompt behind it. The old parked gate
//!    dropped that output on the floor ("Build finished successfully…"
//!    existed in the agent's session data, was absent from zeron's doc).
//! 3. A steer ("what about now") becomes the next turn — the agent answers,
//!    and the turn-end reply is LOST upstream. No Done ever arrives; the
//!    session read Working forever (the live heartbeat defeats the 45s
//!    staleness gate by design, and there is no per-turn timeout).
//!
//! The fixes under test: parked sessions RESUME on self-continued output
//! (fold it, show Working), and the quiesce watchdog settles any turn whose
//! stream goes silent after completed output with nothing in flight.
//! Self-continued parks stay Idle. A prompt or steer turn that goes quiet
//! is a forced settle: Failed in the sidebar and an ErrorChip in the
//! transcript, not a clean Completed finish.

use std::sync::{Arc, Once};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, mpsc};

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry, SteerOutcome};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode, ToolCall,
};

const CHAT: &str = "chat-quiesce";
/// Watchdog window for every test in this file (the process-global env knob
/// is set once, before any engine assembles).
const QUIESCE_MS: u64 = 300;

fn init_quiesce_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: called before any engine (and thus any reader of the var)
        // exists in this test process; all tests share the one value.
        unsafe { std::env::set_var("ZERON_TURN_QUIESCE_MS", QUIESCE_MS.to_string()) };
    });
}

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("hs-q".into()),
    }
}

fn session_started() -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-q".into(),
        assistant_message_id: "a-q".into(),
    }
}

fn text(t: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: t.into() }
}

/// Feed-by-hand harness: the test pushes events through a channel, so it can
/// model turn boundaries, self-continuation, and a LOST turn-end exactly.
/// Accepted steers are confirmed with a `Steered` boundary, like the ACP
/// adapters do. The feed is served only to the test's own dispatch (matched
/// by prompt) — the engine's auto-titler also runs this harness, and gets an
/// immediately-completed empty stream instead.
struct FeedHarness {
    main_prompt: String,
    feed: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
}

#[async_trait]
impl Harness for FeedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Feed"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        mut controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if request.prompt != self.main_prompt {
            // The auto-titler's side run: complete instantly with nothing.
            let events = vec![Ok(done(DoneStatus::Completed))];
            return Ok(futures::stream::iter(events).boxed());
        }
        let mut feed = self
            .feed
            .lock()
            .await
            .take()
            .expect("FeedHarness serves the main dispatch once per test");
        let (tx, rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(64);
        tokio::spawn(async move {
            let mut steering_open = true;
            loop {
                tokio::select! {
                    // Steers first: a Steered boundary always precedes feed
                    // events the test sends after steering (determinism).
                    biased;
                    steer = controls.steering.recv(), if steering_open => match steer {
                        Some(_) => {
                            let boundary = AgentEvent::Steered {
                                assistant_message_id: None,
                                next_assistant_message_id: None,
                            };
                            if tx.send(Ok(boundary)).await.is_err() {
                                return;
                            }
                        }
                        None => steering_open = false,
                    },
                    event = feed.recv() => match event {
                        Some(event) => {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    },
                }
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct Rig {
    core: EngineCore,
    feed: mpsc::UnboundedSender<AgentEvent>,
    _dir: tempfile::TempDir,
}

fn assemble(main_prompt: &str) -> Rig {
    init_quiesce_env();
    let (feed, rx) = mpsc::unbounded_channel();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(FeedHarness {
        main_prompt: main_prompt.into(),
        feed: Mutex::new(Some(rx)),
    }));
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    Rig {
        core,
        feed,
        _dir: dir,
    }
}

fn status(core: &EngineCore) -> Option<SessionStatus> {
    core.sessions.session_status(CHAT).map(|s| s.status)
}

/// Tolerant read (see e2e.rs): a snapshot mid-segment-write deserializes with
/// fields missing — treat that instant as "not yet".
fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn assistant_texts(core: &EngineCore) -> Vec<(String, Option<MessageStatus>)> {
    entries(core)
        .into_iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .map(|e| {
            let text = e
                .parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            (text, e.status)
        })
        .collect()
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Steps 1+2 of the incident: a completed turn parks the session; the agent
/// then self-continues (background-task re-invocation) with no prompt. The
/// output must land in the transcript (it used to be dropped), the session
/// must read Working while it streams, and — with no Done ever coming for a
/// self-started turn — the watchdog must settle it back to Idle.
#[tokio::test]
async fn parked_self_continuation_folds_and_requiesces() {
    let rig = assemble("pull the app and benchmark it");
    rig.core
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            run_request("pull the app and benchmark it"),
            None,
        )
        .await
        .expect("dispatch");

    // Turn 1 completes normally → parked Idle.
    rig.feed.send(session_started()).unwrap();
    rig.feed.send(text("Still going, and healthy.")).unwrap();
    rig.feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "park after Done",
    )
    .await;

    // Self-continuation: streamed output with NO turn behind it, arriving
    // well past the resume gate (a real one follows a whole agent round
    // trip — the incident's came five minutes after the park).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    rig.feed
        .send(text("Build finished successfully. Launching the app."))
        .unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Working),
        "parked session resumes Working on self-continued output",
    )
    .await;

    // No Done will ever come for a self-started turn: the watchdog parks it.
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "watchdog re-parks the self-continued turn",
    )
    .await;

    // The self-continued output is in the doc as its own COMPLETE entry —
    // this exact text was lost in the incident.
    let texts = assistant_texts(&rig.core);
    assert!(
        texts.iter().any(|(t, s)| {
            t.contains("Build finished successfully") && *s == Some(MessageStatus::Complete)
        }),
        "self-continued output must fold into a complete transcript entry, got {texts:#?}"
    );

    rig.core.sessions.shutdown().await;
}

/// A first-prompt turn that streams then goes silent (no Done) must not look
/// like a clean finish. The watchdog parks Failed and stamps the quiet chip.
#[tokio::test]
async fn first_prompt_quiet_settle_is_visible() {
    let rig = assemble("ship the connecting-state fix");
    rig.core
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            run_request("ship the connecting-state fix"),
            None,
        )
        .await
        .expect("dispatch");

    rig.feed.send(session_started()).unwrap();
    rig.feed.send(text("Working on it.")).unwrap();

    wait_for(
        || status(&rig.core) == Some(SessionStatus::Errored),
        "first-prompt quiet settle is Failed, not Idle",
    )
    .await;
    assert!(
        entries(&rig.core).iter().any(|e| {
            e.role == MessageRole::Assistant
                && e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Error { message, .. }
                            if message == "Turn settled — agent went quiet."
                    )
                })
        }),
        "forced settle must stamp the quiet ErrorChip, got {:#?}",
        entries(&rig.core)
    );

    rig.core.sessions.shutdown().await;
}


/// Step 3 of the incident: a steer becomes the next turn, the agent answers,
/// and the turn-end reply is LOST. The session must not read Working forever
/// — the watchdog settles it, and the answer text survives in the doc.
#[tokio::test]
async fn missing_turn_end_settles_instead_of_working_forever() {
    let rig = assemble("pull the app and benchmark it");
    rig.core
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            run_request("pull the app and benchmark it"),
            None,
        )
        .await
        .expect("dispatch");

    rig.feed.send(session_started()).unwrap();
    rig.feed.send(text("Cloned and building.")).unwrap();
    rig.feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "park after Done",
    )
    .await;

    // "what about now" — routed into the live run's mailbox; the harness
    // confirms it with a Steered boundary, which re-arms Working.
    let outcome = rig
        .core
        .sessions
        .steer(CHAT, "what about now", None)
        .await
        .expect("steer");
    assert!(matches!(outcome, SteerOutcome::Accepted));
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Working),
        "steer boundary re-arms Working",
    )
    .await;

    // The agent answers… and its Done is lost upstream. Nothing else comes.
    rig.feed.send(text("Done — here are the results.")).unwrap();

    // Old behavior: Working forever (heartbeat keeps the row fresh; no
    // turn timeout). New behavior: the watchdog settles the turn as a
    // visible failure, not a clean Idle/Completed finish.
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Errored),
        "watchdog settles the turn whose Done was lost",
    )
    .await;
    let texts = assistant_texts(&rig.core);
    assert!(
        texts.iter().any(|(t, s)| {
            t.contains("here are the results") && *s == Some(MessageStatus::Complete)
        }),
        "the lost-Done turn's answer must still finalize in the doc, got {texts:#?}"
    );
    assert!(
        entries(&rig.core).iter().any(|e| {
            e.role == MessageRole::Assistant
                && e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Error { message, .. }
                            if message == "Turn settled — agent went quiet."
                    )
                })
        }),
        "forced settle must stamp the quiet ErrorChip, got {:#?}",
        entries(&rig.core)
    );

    rig.core.sessions.shutdown().await;
}

/// The guard the design comment insists on: a long-running tool call is
/// legitimately silent for minutes — an unresolved tool part must hold the
/// watchdog off no matter how long the stream is quiet.
#[tokio::test]
async fn open_tool_call_never_quiesces() {
    let rig = assemble("run the slow build");
    rig.core
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            run_request("run the slow build"),
            None,
        )
        .await
        .expect("dispatch");

    rig.feed.send(session_started()).unwrap();
    rig.feed.send(text("Kicking off the build.")).unwrap();
    rig.feed
        .send(AgentEvent::ToolCall {
            id: "tool-slow".into(),
            call: ToolCall::Exec {
                command: "cargo build --release".into(),
            },
        })
        .unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Working),
        "run starts Working",
    )
    .await;

    // Several full watchdog windows of silence mid-tool-call.
    tokio::time::sleep(Duration::from_millis(QUIESCE_MS * 4)).await;
    assert_eq!(
        status(&rig.core),
        Some(SessionStatus::Working),
        "an unresolved tool call must never quiesce"
    );

    // The tool resolves and the turn ends normally.
    rig.feed
        .send(AgentEvent::ToolResult {
            id: "tool-slow".into(),
            is_error: false,
            output: None,
            diff: None,
        })
        .unwrap();
    rig.feed.send(text("Build done.")).unwrap();
    rig.feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "clean turn end",
    )
    .await;

    rig.core.sessions.shutdown().await;
}

/// Post-turn noise (the original eternally-running-session bug) must STILL be
/// gated: a stale tool echo for an id folded in a prior segment does not
/// resume a parked session.
#[tokio::test]
async fn stale_tool_echo_stays_parked() {
    let rig = assemble("do a thing");
    rig.core
        .sessions
        .dispatch(CHAT, HarnessId::Mock, run_request("do a thing"), None)
        .await
        .expect("dispatch");

    rig.feed.send(session_started()).unwrap();
    rig.feed
        .send(AgentEvent::ToolCall {
            id: "tool-echoed".into(),
            call: ToolCall::Exec {
                command: "sleep 600".into(),
            },
        })
        .unwrap();
    rig.feed
        .send(AgentEvent::ToolResult {
            id: "tool-echoed".into(),
            is_error: false,
            output: None,
            diff: None,
        })
        .unwrap();
    rig.feed
        .send(text("Started it in the background."))
        .unwrap();
    rig.feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "park after Done",
    )
    .await;

    // A late tool_call_update re-emitted as a full ToolCall for the OLD id —
    // sent past the resume gate, so it is the seen-tools guard (not the
    // gate) keeping the session parked.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    rig.feed
        .send(AgentEvent::ToolCall {
            id: "tool-echoed".into(),
            call: ToolCall::Exec {
                command: "sleep 600".into(),
            },
        })
        .unwrap();

    // Give the echo ample time to (wrongly) resume the session.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        status(&rig.core),
        Some(SessionStatus::Idle),
        "a stale tool echo must not resume a parked session"
    );

    rig.core.sessions.shutdown().await;
}
