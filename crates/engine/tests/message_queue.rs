//! The pending-message queue: what happens to a message typed while the agent
//! is busy, and how it eventually reaches the agent.
//!
//! The policy under test (`DocHost::drain_queue`):
//! - idle agent → the queue drains immediately, in order;
//! - busy agent that only takes input at a turn boundary → the queue HOLDS,
//!   and flushes when the turn ends;
//! - busy agent, including one that takes input mid-turn → held until turn end;
//! - "send now" → interrupts whatever is running.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{
    MessagePart, MessageRole, QueueDeliveryGate, SessionCommandPayload, SessionMessageEntry,
};
use zeron_engine::doc_host::{
    BeginQueueEditOutcome, FinishQueueEditAction, FinishQueueEditOutcome,
};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputQuestion,
};

const CHAT: &str = "chat-queue";

/// A turn that does not end until the test says so, so "the agent is busy" is
/// a state the test controls rather than races.
struct HeldHarness {
    steering: SteeringMode,
    finish: tokio::sync::broadcast::Sender<()>,
    prompts: Arc<Mutex<Vec<String>>>,
    requests: Mutex<Vec<RunRequest>>,
    /// Park the first turn on a question instead of just hanging, so the chat
    /// sits in `AwaitingInput` rather than `Working`.
    asks: bool,
}

impl HeldHarness {
    fn new(steering: SteeringMode) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        Self::build(steering, false)
    }

    fn asking(steering: SteeringMode) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        Self::build(steering, true)
    }

    fn build(steering: SteeringMode, asks: bool) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let (finish, _) = tokio::sync::broadcast::channel(16);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                steering,
                finish,
                prompts: prompts.clone(),
                requests: Mutex::new(Vec::new()),
                asks,
            }),
            prompts,
        )
    }
}

#[async_trait]
impl Harness for HeldHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Held"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        self.steering
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
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        self.requests.lock().unwrap().push(request.clone());
        if self.asks {
            // Only the engine can mint a request id it will honour, so the
            // question has to go through controls rather than the stream.
            let _answer = (controls.request_input)(vec![UserInputQuestion {
                id: "q1".into(),
                header: "Choose".into(),
                question: "which one?".into(),
                options: vec!["a".into(), "b".into()],
                multi_select: false,
            }]);
        }
        let mut finish = self.finish.subscribe();
        let mut steering = controls.steering;
        let started = futures::stream::iter(vec![Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "sess-queue".into(),
            assistant_message_id: format!("a-{}", request.prompt),
        })]);
        let done = futures::stream::once(async move {
            loop {
                tokio::select! {
                    _ = finish.recv() => {
                        return Ok(AgentEvent::Done {
                            status: DoneStatus::Completed,
                            result: None,
                            error: None,
                            session_id: Some("sess-queue".into()),
                        });
                    }
                    steer = steering.recv() => {
                        if steer.is_none() {
                            return Ok(AgentEvent::Done {
                                status: DoneStatus::Completed,
                                result: None,
                                error: None,
                                session_id: Some("sess-queue".into()),
                            });
                        }
                    }
                }
            }
        });
        Ok(started.chain(done).boxed())
    }
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
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn queue_texts(core: &EngineCore) -> Vec<String> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_queue().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|item| item.text)
        .collect()
}

fn user_messages(core: &EngineCore) -> Vec<String> {
    let entries: Vec<SessionMessageEntry> = core
        .doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .map(|e| {
            e.parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

fn user_message_id(core: &EngineCore, text: &str) -> Option<String> {
    core.doc_host
        .open(CHAT)
        .ok()?
        .doc()
        .read_entries()
        .ok()?
        .into_iter()
        .find(|entry| {
            entry.role == MessageRole::User
                && entry.parts.iter().any(
                    |part| matches!(part, MessagePart::Text { text: body, .. } if body == text),
                )
        })
        .map(|entry| entry.id)
}

async fn setup(steering: SteeringMode) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    setup_with(HeldHarness::new(steering)).await
}

/// [`setup`] with a harness whose turn parks on a question.
async fn setup_asking(
    steering: SteeringMode,
) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    setup_with(HeldHarness::asking(steering)).await
}

async fn setup_with(
    built: (Arc<HeldHarness>, Arc<Mutex<Vec<String>>>),
) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    let tmp = tempfile::tempdir().unwrap();
    // Leak the tempdir guard: the engine outlives this helper and the test only
    // cares that the path is unique per run.
    let path = tmp.keep();
    let (harness, prompts) = built;
    let core = assemble_at(&path.join("data"), harness.clone());
    create_chat(&core).await;
    (core, harness, prompts)
}

fn assemble_at(path: &std::path::Path, harness: Arc<HeldHarness>) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    EngineCore::assemble(path, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

async fn create_chat(core: &EngineCore) {
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": core.device_id,
            }),
        )
        .await
        .expect("createChat");
    // Pre-title so the auto-titler never dispatches a harness request of its own.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");
}

/// Nothing is running, so a queued message is just a message: it goes out at
/// once, and the queue is empty again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_drains_immediately_when_the_agent_is_idle() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    let queued_id = core
        .doc_host
        .queue_message(CHAT, "first", Vec::new())
        .expect("queue message");

    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "first"),
        "the queued message to dispatch",
    )
    .await;
    wait_for(
        || user_message_id(&core, "first").as_deref() == Some(queued_id.as_str()),
        "the queue id to become the transcript message id",
    )
    .await;
    wait_for(|| queue_texts(&core).is_empty(), "the queue to empty").await;

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A turn-boundary agent cannot take a message mid-turn, so the queue holds it —
/// visible, editable — and sends it when the turn ends. This is the case the
/// composer's steering warning is about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_holds_during_a_turn_and_flushes_in_order_at_its_end() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    // Start a turn and let it hang.
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let second_id = core
        .doc_host
        .queue_message(CHAT, "second", Vec::new())
        .expect("queue second");
    let third_id = core
        .doc_host
        .queue_message(CHAT, "third", Vec::new())
        .expect("queue third");

    // Both must still be sitting there: the agent is busy and cannot be steered.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(queue_texts(&core), vec!["second", "third"]);
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "second"));

    // Turn ends → the queue flushes, head first.
    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "second"),
        "the queue to flush at turn end",
    )
    .await;
    wait_for(
        || user_message_id(&core, "second").as_deref() == Some(second_id.as_str()),
        "the held queue id to become the next-turn message id",
    )
    .await;
    assert_eq!(
        queue_texts(&core),
        vec!["third"],
        "only the head goes: the next turn is now running"
    );

    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "third"),
        "the rest of the queue to flush",
    )
    .await;
    wait_for(
        || user_message_id(&core, "third").as_deref() == Some(third_id.as_str()),
        "the next held queue id to remain stable too",
    )
    .await;
    let order = prompts.lock().unwrap().clone();
    let sent: Vec<&String> = order
        .iter()
        .filter(|p| ["opening", "second", "third"].contains(&p.as_str()))
        .collect();
    assert_eq!(
        sent,
        vec!["opening", "second", "third"],
        "queued messages keep their order"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Quitting the host must neither release the queue through the interrupt's
/// Idle transition nor release it when the persisted document is reopened.
/// An explicit queue action remains the thaw gesture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_restores_the_queue_frozen_until_an_explicit_send() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let (before_harness, before_prompts) = HeldHarness::new(SteeringMode::TurnBoundary);
    let before = assemble_at(&data_dir, before_harness);
    create_chat(&before).await;

    before
        .doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || {
            before_prompts
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "opening")
        },
        "the opening turn to start",
    )
    .await;
    let queued_id = before
        .doc_host
        .queue_message(CHAT, "after restart", Vec::new())
        .expect("queue recovered message");
    wait_for(
        || queue_texts(&before) == vec!["after restart"],
        "the follow-up to remain queued",
    )
    .await;

    before.shutdown().await;
    drop(before);

    let (after_harness, after_prompts) = HeldHarness::new(SteeringMode::TurnBoundary);
    let after = assemble_at(&data_dir, after_harness);
    // Materializing the persisted chat starts its background drain task. Give
    // that task time to prove the restored row stays frozen.
    assert_eq!(queue_texts(&after), vec!["after restart"]);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        after_prompts.lock().unwrap().is_empty(),
        "startup must not dispatch a recovered queue"
    );
    assert_eq!(queue_texts(&after), vec!["after restart"]);

    assert!(
        after
            .doc_host
            .send_queued_now(CHAT, &queued_id)
            .await
            .expect("explicit send succeeds")
    );
    wait_for(
        || {
            after_prompts
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "after restart")
        },
        "the explicit send to thaw the recovered queue",
    )
    .await;

    after.shutdown().await;
}

/// Cancel is not a normal turn boundary: it freezes the visible queue instead
/// of immediately promoting its head into a new inference. The freeze survives
/// incidental queue edits and is lifted by an explicit send action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_turn_freezes_the_queue_until_an_explicit_send() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    for text in ["second", "third"] {
        core.doc_host
            .queue_message(CHAT, text, Vec::new())
            .expect("queue follow-up");
    }
    let second_id = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap()[0]
        .id
        .clone();

    core.doc_host
        .queue_command(CHAT, SessionCommandPayload::Interrupt {})
        .expect("queue interrupt");
    wait_for(
        || !core.sessions.turn_in_flight(CHAT),
        "the interrupted turn to settle",
    )
    .await;

    core.doc_host
        .update_queued_message(CHAT, &second_id, "second edited")
        .expect("edit frozen row");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(queue_texts(&core), vec!["second edited", "third"]);
    assert!(
        !prompts.lock().unwrap().iter().any(|p| p == "second edited"),
        "Cancel must not turn a queued row into a new inference"
    );

    core.doc_host
        .send_queued_now(CHAT, &second_id)
        .await
        .expect("explicit queue send");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "second edited"),
        "the explicitly selected row to start",
    )
    .await;
    assert_eq!(queue_texts(&core), vec!["third"]);

    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "third"),
        "normal draining to resume after the explicit send",
    )
    .await;
    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A `Steer` command asks for the running turn directly — a client that decided
/// for itself, and the path a question's follow-up prompt takes. It obeys the
/// same rule as a typed message: a turn-boundary agent's mailbox is not read
/// mid-turn, so the prompt is held rather than posted into it.
///
/// Posting it anyway is the 2026-08-13 report: on `cursor-agent` the follow-up
/// went into the mailbox, the turn ended interrupted, and the message sat in the
/// transcript looking sent with the agent never seeing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_command_holds_for_an_agent_that_takes_no_mid_turn_prompt() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the turn to start",
    )
    .await;

    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "and also this".into(),
                message_id: Some("m-steer".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || queue_texts(&core) == vec!["and also this"],
        "the steer to be held in the queue",
    )
    .await;
    // Held means held: not shown as sent, and not with the agent.
    assert!(!user_messages(&core).iter().any(|m| m == "and also this"));
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "and also this"));

    // And it goes on its own when the turn ends — nobody re-sends it.
    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "and also this"),
        "the held steer to flush at turn end",
    )
    .await;
    assert!(queue_texts(&core).is_empty());

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Even agents that support mid-turn input deliver queue rows one turn at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_text_waits_for_a_steerable_turn_even_with_legacy_policy() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .unwrap();
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "first turn",
    )
    .await;

    // Omitted and explicitly false policies from old clients must both wait.
    core.doc_host
        .queue_message(CHAT, "first queued", Vec::new())
        .unwrap();
    core.doc_host
        .queue_message_with_behavior(CHAT, "second queued", Vec::new(), false)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(queue_texts(&core), vec!["first queued", "second queued"]);
    assert_eq!(user_messages(&core), vec!["opening"]);

    harness.finish.send(()).unwrap();
    wait_for(
        || user_messages(&core).iter().any(|m| m == "first queued"),
        "first queued turn",
    )
    .await;
    assert_eq!(queue_texts(&core), vec!["second queued"]);
    assert!(!user_messages(&core).iter().any(|m| m == "second queued"));
    harness.finish.send(()).unwrap();
    wait_for(
        || user_messages(&core).iter().any(|m| m == "second queued"),
        "second queued turn",
    )
    .await;
    assert!(queue_texts(&core).is_empty());
    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// The legacy explicit-steer RPC remains compatible with older clients;
/// current clients offer only Send now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_policy_keeps_a_steerable_message_visible_until_steer_now() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;
    let client = zeron_rpc::memory_client(core.rpc_service());

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let reply = client
        .call(
            zeron_rpc::methods::QUEUE_MESSAGE,
            serde_json::json!({
                "chatId": CHAT,
                "text": "hold this",
                "holdForTurnEnd": true,
            }),
        )
        .await
        .expect("queue held message");
    let id = reply["id"].as_str().expect("queue id").to_string();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(queue_texts(&core), vec!["hold this"]);
    assert!(!user_messages(&core).iter().any(|m| m == "hold this"));

    let reply = client
        .call(
            zeron_rpc::methods::STEER_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "chatId": CHAT, "id": id }),
        )
        .await
        .expect("steer now");
    assert_eq!(reply["sent"], true);
    wait_for(
        || user_messages(&core).iter().any(|m| m == "hold this"),
        "the held message to steer",
    )
    .await;
    assert!(queue_texts(&core).is_empty());

    let _ = harness.finish.send(());
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_now_leaves_the_row_when_the_agent_cannot_steer_mid_turn() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;
    let id = core
        .doc_host
        .queue_message(CHAT, "still queued", Vec::new())
        .expect("queue held message");

    assert!(core.doc_host.steer_queued_now(CHAT, &id).await.is_err());
    assert_eq!(queue_texts(&core), vec!["still queued"]);

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A provider's queue action remains `Steer` even if the active turn finishes
/// before the click lands. With nothing left to interrupt, the selected row
/// becomes the next turn and unpauses normal queue draining.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_now_starts_the_next_turn_when_the_previous_turn_is_already_idle() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let client = zeron_rpc::memory_client(core.rpc_service());
    let reply = client
        .call(
            zeron_rpc::methods::QUEUE_MESSAGE,
            serde_json::json!({
                "chatId": CHAT,
                "text": "after cancel",
                "holdForTurnEnd": true,
            }),
        )
        .await
        .expect("queue held message");
    let id = reply["id"].as_str().expect("queue id").to_string();

    core.doc_host
        .queue_command(CHAT, SessionCommandPayload::Interrupt {})
        .expect("queue interrupt");
    wait_for(
        || !core.sessions.turn_in_flight(CHAT),
        "the interrupted turn to settle",
    )
    .await;
    assert_eq!(queue_texts(&core), vec!["after cancel"]);

    assert!(
        core.doc_host
            .steer_queued_now(CHAT, &id)
            .await
            .expect("non-interrupting queue promotion")
    );
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "after cancel"),
        "the selected row to start the next turn",
    )
    .await;
    assert_eq!(
        user_message_id(&core, "after cancel").as_deref(),
        Some(id.as_str()),
        "promoting an idle queued row preserves its transcript identity"
    );
    assert!(queue_texts(&core).is_empty());

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Attachments never steer: the steer path carries a prompt and nothing else,
/// so a message with files waits for a turn that can inline them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_with_attachments_holds_even_for_a_steerable_agent() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "with a file", vec!["att-1".into()])
        .expect("queue attachment message");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue_texts(&core),
        vec!["with a file"],
        "a message carrying files must not be steered"
    );
    assert!(
        user_messages(&core)
            .iter()
            .all(|message| !message.contains("with a file")),
        "a held attachment row must not appear in the transcript before dispatch"
    );

    let _ = harness.finish.send(());
    wait_for(
        || {
            prompts.lock().unwrap().iter().any(|p| {
                p == "with a file\n\nAttached images (local files — open them to view):\n- att-1"
            })
        },
        "the held message to flush at turn end",
    )
    .await;
    assert!(
        user_messages(&core)
            .iter()
            .any(|message| message.contains("Attached images (local files")),
        "the transport trailer is materialized only when the queued row dispatches"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// "Send now" interrupts the running turn and sends that one message, leaving
/// the rest queued. The UI selects this action for providers that cannot steer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_now_interrupts_the_running_turn() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let first = core
        .doc_host
        .queue_message(CHAT, "urgent", Vec::new())
        .expect("queue urgent");
    core.doc_host
        .queue_message(CHAT, "later", Vec::new())
        .expect("queue later");

    assert!(
        core.doc_host
            .send_queued_now(CHAT, &first)
            .await
            .expect("send now"),
        "send now takes the row"
    );
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "urgent"),
        "the urgent message to reach the agent",
    )
    .await;
    assert_eq!(
        queue_texts(&core),
        vec!["later"],
        "the rest of the queue is untouched"
    );

    // A row someone else already took is not an error, just `false`.
    assert!(
        !core
            .doc_host
            .send_queued_now(CHAT, &first)
            .await
            .expect("second send now")
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Editing a queued message to nothing is the delete gesture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_a_queued_message_to_empty_removes_it() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let id = core
        .doc_host
        .queue_message(CHAT, "typo", Vec::new())
        .expect("queue typo");
    assert!(
        core.doc_host
            .update_queued_message(CHAT, &id, "fixed")
            .expect("edit")
    );
    assert_eq!(queue_texts(&core), vec!["fixed"]);

    assert!(
        core.doc_host
            .update_queued_message(CHAT, &id, "   ")
            .expect("empty edit")
    );
    assert!(
        queue_texts(&core).is_empty(),
        "emptying a queued message deletes it"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A positive removal acknowledgement is stronger than a local CRDT delete:
/// the host has serialized it against automatic drain, so ending the current
/// turn cannot materialize the cancelled row in the transcript afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_removal_cannot_materialize_after_turn_end() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let id = core
        .doc_host
        .queue_message(CHAT, "cancelled", Vec::new())
        .expect("queue cancelled row");
    assert!(
        core.doc_host
            .remove_queued_message(CHAT, &id)
            .await
            .expect("host removal acknowledgement")
    );

    let _ = harness.finish.send(());
    let handle = core.doc_host.open(CHAT).expect("open chat doc");
    core.doc_host.drain_queue(&handle).await;

    assert!(queue_texts(&core).is_empty());
    assert!(
        !prompts.lock().unwrap().iter().any(|p| p == "cancelled"),
        "an acknowledged removal must never reach the harness"
    );
    assert!(
        !user_messages(&core).iter().any(|p| p == "cancelled"),
        "an acknowledged removal must never reach the transcript"
    );

    core.shutdown().await;
}

/// Reordering, over the RPC surface the UIs actually call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_rpc_reorders_and_streams() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    let client = zeron_rpc::memory_client(core.rpc_service());

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let mut rx = client
        .subscribe(
            zeron_rpc::methods::WATCH_QUEUE,
            serde_json::json!({ "chatId": CHAT }),
        )
        .await
        .expect("WatchQueue");
    let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("first queue frame")
        .expect("stream open");
    assert_eq!(
        first["items"].as_array().map(Vec::len),
        Some(0),
        "the stream opens with the current queue"
    );

    for text in ["a", "b", "c"] {
        client
            .call(
                zeron_rpc::methods::QUEUE_MESSAGE,
                serde_json::json!({ "chatId": CHAT, "text": text }),
            )
            .await
            .expect("QueueMessage");
    }
    assert_eq!(queue_texts(&core), vec!["a", "b", "c"]);

    let last_id = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    client
        .call(
            zeron_rpc::methods::MOVE_QUEUED_MESSAGE,
            serde_json::json!({ "chatId": CHAT, "id": last_id, "toIndex": 0 }),
        )
        .await
        .expect("MoveQueuedMessage");
    assert_eq!(queue_texts(&core), vec!["c", "a", "b"]);

    client
        .call(
            zeron_rpc::methods::REMOVE_QUEUED_MESSAGE,
            serde_json::json!({ "chatId": CHAT, "id": last_id }),
        )
        .await
        .expect("RemoveQueuedMessage");
    assert_eq!(queue_texts(&core), vec!["a", "b"]);

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Even a malformed or version-skewed UI must not make a non-host engine take
/// a shared queue row and start the chat on the wrong machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_queue_delivery_is_rejected_off_host_without_taking_the_row() {
    let (core, harness, _prompts) = setup(SteeringMode::StepBoundary).await;
    core.workspace
        .set_chat_host(CHAT, "device-remote")
        .expect("move chat host");
    let id = core
        .doc_host
        .queue_message_with_behavior(CHAT, "remote-only", Vec::new(), true)
        .expect("queue held row");

    let send_error = core
        .doc_host
        .send_queued_now(CHAT, &id)
        .await
        .expect_err("non-host send-now must fail");
    assert!(send_error.to_string().contains("does not host chat"));
    assert_eq!(queue_texts(&core), vec!["remote-only"]);

    let steer_error = core
        .doc_host
        .steer_queued_now(CHAT, &id)
        .await
        .expect_err("non-host steer-now must fail");
    assert!(steer_error.to_string().contains("does not host chat"));

    let remove_error = core
        .doc_host
        .remove_queued_message(CHAT, &id)
        .await
        .expect_err("non-host removal must fail");
    assert!(remove_error.to_string().contains("does not host chat"));
    assert_eq!(queue_texts(&core), vec!["remote-only"]);

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// The regression the review caught: `drain_queue` runs from both the
/// doc-change task and the turn-end status watcher, and there is nothing about
/// those two callers that keeps them apart. Driven concurrently against an idle
/// agent, an unserialized drain has both take a different head across the
/// `dispatch` await and both send — the queue empties in one go, which is the
/// "looks sent" failure the whole feature exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_drains_release_one_message() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    let handle = core.doc_host.open(CHAT).expect("open chat");

    for text in ["first", "second", "third"] {
        core.doc_host
            .queue_message(CHAT, text, Vec::new())
            .expect("queue");
    }

    tokio::join!(
        core.doc_host.drain_queue(&handle),
        core.doc_host.drain_queue(&handle),
        core.doc_host.drain_queue(&handle),
    );

    wait_for(
        || !prompts.lock().unwrap().is_empty(),
        "the released message to reach the agent",
    )
    .await;
    // Long enough for a second escapee to show up if the drains interleaved.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue_texts(&core),
        vec!["second", "third"],
        "one drain released the head; the others found a busy agent"
    );
    assert_eq!(
        prompts.lock().unwrap().clone(),
        vec!["first".to_string()],
        "the agent was handed exactly one prompt"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// An agent parked on a question still owns the turn. The composer queues on
/// that state, so the drain has to hold there too — reading `AwaitingInput` as
/// idle sends the follow-up as a fresh turn and abandons the question.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_holds_while_the_agent_waits_on_a_question() {
    let (core, harness, prompts) = setup_asking(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;
    wait_for(
        || {
            core.sessions
                .session_status(CHAT)
                .is_some_and(|s| s.status == zeron_proto::SessionStatus::AwaitingInput)
        },
        "the agent to park on its question",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "follow-up", Vec::new())
        .expect("queue follow-up");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        queue_texts(&core),
        vec!["follow-up"],
        "the follow-up waits for the question to be answered"
    );
    assert!(
        !prompts.lock().unwrap().iter().any(|p| p == "follow-up"),
        "no fresh turn started under the parked question"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Core regression: once BeginEdit wins the same lock as the drain, ending the
/// current turn cannot promote the row until the lease is explicitly resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acquired_edit_blocks_turn_end_and_commit_sends_the_new_text() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "opening turn",
    )
    .await;
    let id = core
        .doc_host
        .queue_message(CHAT, "old text", Vec::new())
        .expect("queue editable row");

    let BeginQueueEditOutcome::Acquired {
        lease_id,
        base_text_hash,
        ..
    } = core
        .doc_host
        .begin_queued_message_edit(CHAT, &id, "phone", "view-1")
        .await
        .expect("begin edit")
    else {
        panic!("edit must be acquired");
    };
    let _ = harness.finish.send(());
    wait_for(
        || !core.sessions.turn_in_flight(CHAT),
        "opening turn to end",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(queue_texts(&core), vec!["old text"]);
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "old text"));
    assert!(core.doc_host.send_queued_now(CHAT, &id).await.is_err());

    assert!(matches!(
        core.doc_host
            .finish_queued_message_edit(
                CHAT,
                &id,
                &lease_id,
                FinishQueueEditAction::Commit,
                Some("new text"),
                Some(&base_text_hash),
            )
            .await
            .expect("finish edit"),
        FinishQueueEditOutcome::Committed
    ));
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "new text"),
        "edited row to dispatch",
    )
    .await;
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "old text"));

    let _ = harness.finish.send(());
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_editor_wins_and_an_expired_edit_requires_review() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "opening turn",
    )
    .await;
    let id = core
        .doc_host
        .queue_message(CHAT, "review me", Vec::new())
        .expect("queue row");

    let (a, b) = tokio::join!(
        core.doc_host
            .begin_queued_message_edit(CHAT, &id, "a", "view-a"),
        core.doc_host
            .begin_queued_message_edit(CHAT, &id, "b", "view-b"),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BeginQueueEditOutcome::Acquired { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BeginQueueEditOutcome::Locked { .. }))
            .count(),
        1
    );

    // Force the persisted deadline into the past; turn-end must convert it to
    // ReviewRequired, never treat expiry as permission to send.
    let row = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap()[0]
        .clone();
    let QueueDeliveryGate::Editing {
        lease_id,
        owner_device_id,
        base_text_hash,
        ..
    } = row.delivery_gate.unwrap()
    else {
        panic!("editing gate expected");
    };
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .set_queued_delivery_gate(
            &id,
            Some(&QueueDeliveryGate::Editing {
                lease_id,
                owner_device_id,
                owner_instance_id: "expired".into(),
                acquired_at_ms: 0,
                expires_at_ms: 0,
                base_text_hash,
            }),
        )
        .unwrap();
    let _ = harness.finish.send(());
    wait_for(
        || {
            core.doc_host
                .open(CHAT)
                .unwrap()
                .doc()
                .read_queue()
                .unwrap()
                .first()
                .is_some_and(|row| {
                    matches!(
                        row.delivery_gate,
                        Some(QueueDeliveryGate::ReviewRequired { .. })
                    )
                })
        },
        "expired edit to require review",
    )
    .await;
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "review me"));

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protected_edit_rpc_round_trips_its_camel_case_protocol() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "opening turn",
    )
    .await;
    let id = core
        .doc_host
        .queue_message(CHAT, "rpc edit", Vec::new())
        .expect("queue row");
    let client = zeron_rpc::memory_client(core.rpc_service());
    let begin = client
        .call(
            zeron_rpc::methods::BEGIN_QUEUED_MESSAGE_EDIT,
            serde_json::json!({
                "chatId": CHAT,
                "id": id,
                "editorDeviceId": "phone",
                "editorInstanceId": "view-1",
            }),
        )
        .await
        .expect("begin edit rpc");
    assert_eq!(begin["outcome"], "acquired");
    assert!(begin["baseTextHash"].as_str().is_some());

    let finish = client
        .call(
            zeron_rpc::methods::FINISH_QUEUED_MESSAGE_EDIT,
            serde_json::json!({
                "chatId": CHAT,
                "id": id,
                "leaseId": begin["leaseId"],
                "action": "commit",
                "text": "rpc revised",
                "expectedTextHash": begin["baseTextHash"],
            }),
        )
        .await
        .expect("finish edit rpc");
    assert_eq!(finish["outcome"], "committed");
    // The current turn is still live, so the revised row remains queued.
    assert_eq!(queue_texts(&core), vec!["rpc revised"]);

    let _ = harness.finish.send(());
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composer_edit_commits_attachments_in_place_and_cancel_preserves_them() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .unwrap();
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "opening turn",
    )
    .await;
    core.doc_host
        .queue_message(CHAT, "before", Vec::new())
        .unwrap();
    let id = core
        .doc_host
        .queue_message(CHAT, "original", vec!["old.png".into()])
        .unwrap();
    core.doc_host
        .queue_message(CHAT, "after", Vec::new())
        .unwrap();
    let BeginQueueEditOutcome::Acquired {
        lease_id,
        base_text_hash,
        attachments,
        ..
    } = core
        .doc_host
        .begin_queued_message_edit(CHAT, &id, "desktop", "composer")
        .await
        .unwrap()
    else {
        panic!("edit must be acquired");
    };
    assert_eq!(attachments, vec!["old.png"]);
    let paths = vec!["new.png".to_string()];
    assert!(matches!(
        core.doc_host
            .finish_queued_message_edit_with_attachments(
                CHAT,
                &id,
                &lease_id,
                FinishQueueEditAction::Commit,
                Some("revised"),
                Some(&base_text_hash),
                Some(&paths),
            )
            .await
            .unwrap(),
        FinishQueueEditOutcome::Committed
    ));
    assert_eq!(queue_texts(&core), vec!["before", "revised", "after"]);
    let BeginQueueEditOutcome::Acquired {
        lease_id,
        attachments,
        ..
    } = core
        .doc_host
        .begin_queued_message_edit(CHAT, &id, "desktop", "composer")
        .await
        .unwrap()
    else {
        panic!("second edit must be acquired");
    };
    assert_eq!(attachments, paths);
    assert!(matches!(
        core.doc_host
            .finish_queued_message_edit_with_attachments(
                CHAT,
                &id,
                &lease_id,
                FinishQueueEditAction::Cancel,
                None,
                None,
                Some(&[]),
            )
            .await
            .unwrap(),
        FinishQueueEditOutcome::Cancelled
    ));
    let BeginQueueEditOutcome::Acquired { attachments, .. } = core
        .doc_host
        .begin_queued_message_edit(CHAT, &id, "desktop", "composer")
        .await
        .unwrap()
    else {
        panic!("cancel must release the lease");
    };
    assert_eq!(attachments, paths);
    assert_eq!(queue_texts(&core), vec!["before", "revised", "after"]);
    let _ = harness.finish.send(());
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_queue_dispatch_stays_paused_until_explicit_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let (harness, prompts) = HeldHarness::new(SteeringMode::TurnBoundary);
    let core = assemble_at(tmp.path(), harness.clone());
    let handle = core.doc_host.open(CHAT).unwrap();
    let id = core
        .doc_host
        .queue_message(CHAT, "recover me", Vec::new())
        .unwrap();
    core.doc_host.drain_queue(&handle).await;
    let version = handle.doc().doc().oplog_vv();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(handle.doc().doc().oplog_vv(), version);
    assert_eq!(queue_texts(&core), vec!["recover me"]);
    assert!(prompts.lock().unwrap().is_empty());
    create_chat(&core).await;
    core.doc_host.drain_queue(&handle).await;
    assert_eq!(queue_texts(&core), vec!["recover me"]);
    assert!(core.doc_host.send_queued_now(CHAT, &id).await.unwrap());
    wait_for(|| prompts.lock().unwrap().len() == 1, "explicit recovery").await;
    assert_eq!(user_message_id(&core, "recover me"), Some(id));
    let _ = harness.finish.send(());
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_turn_uses_current_config_at_turn_end_and_send_now() {
    for send_now in [false, true] {
        let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
        let mut config = zeron_proto::ChatConfig {
            harness: HarnessId::Mock,
            model: Some("old-model".into()),
            reasoning: Some(ReasoningLevel::Medium),
            model_options: Default::default(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        core.workspace.set_chat_config(CHAT, &config).unwrap();
        core.doc_host
            .queue_message(CHAT, "opening", Vec::new())
            .unwrap();
        wait_for(|| prompts.lock().unwrap().len() == 1, "first turn").await;
        config.model = Some("new-model".into());
        config.reasoning = None;
        config
            .model_options
            .insert("contextWindow".into(), serde_json::json!("1m"));
        core.workspace.set_chat_config(CHAT, &config).unwrap();
        let id = core
            .doc_host
            .queue_message(CHAT, "follow-up", Vec::new())
            .unwrap();
        if send_now {
            assert!(core.doc_host.send_queued_now(CHAT, &id).await.unwrap());
        } else {
            let _ = harness.finish.send(());
        }
        wait_for(|| prompts.lock().unwrap().len() == 2, "queued turn").await;
        {
            let requests = harness.requests.lock().unwrap();
            assert_eq!(requests[0].model.as_deref(), Some("old-model"));
            assert_eq!(requests[1].model, config.model);
            assert_eq!(requests[1].reasoning, config.reasoning);
            assert_eq!(requests[1].model_options, config.model_options);
        }
        let _ = harness.finish.send(());
        core.shutdown().await;
    }
}
