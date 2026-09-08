//! Blanket dropped-reply settle (`ZERON_ACP_QUIET_SETTLE_MS`), tested with
//! the GROK spec so no adapter-specific evidence (Claude's cost frame,
//! `noRunningTurn` steering reasons) is in play — this is the path every ACP
//! agent gets. Own test binary: the env knob is process-global, and every
//! test here shares the one value.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

const QUIET_MS: u64 = 1200;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set before any harness runs in this test process; all
        // tests in this binary share the one value.
        unsafe { std::env::set_var("ZERON_ACP_QUIET_SETTLE_MS", QUIET_MS.to_string()) };
    });
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("grok-4.5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn run_and_collect(
    harness: AcpHarness,
    prompt: &str,
    timeout: Duration,
) -> Vec<(std::time::Instant, AgentEvent)> {
    let (controls, _steer, _token) = controls();
    let harness = harness.with_executable(fixture_path());
    let stream = harness
        .run(request(prompt), controls)
        .await
        .expect("run starts");
    tokio::time::timeout(timeout, async move {
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push((std::time::Instant::now(), ev.expect("stream event")));
        }
        events
    })
    .await
    .expect("run finished in time")
}

fn dones(events: &[(std::time::Instant, AgentEvent)]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

/// A generic agent whose prompt response is dropped: content streamed, no
/// open tool, then silence. The blanket settle must produce a clean Done off
/// the quiet window, well before the fixture's held-open stream ends.
#[tokio::test]
async fn generic_dropped_reply_settles_off_the_quiet_window() {
    init_env();
    let started = std::time::Instant::now();
    let events = run_and_collect(
        AcpHarness::grok(),
        "scenario:quiet-starve",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    let done_at = events
        .iter()
        .find(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .map(|(t, _)| t.duration_since(started))
        .expect("done asserted above");
    // Window is 1.2s; the fixture holds the stream open for 8s. The Done
    // must come from the settle, not EOF (margin sized for suite load).
    assert!(
        done_at < Duration::from_secs(6),
        "Done at {done_at:?} — should ride the {QUIET_MS}ms quiet window, \
         not the 8s stream EOF"
    );
}

/// The guard: an OPEN tool call makes silence legitimate. The fixture is
/// quiet for ~3x the settle window mid-tool-call, then finishes normally —
/// exactly one Done, arriving from the real response, never a premature
/// synthesized one.
#[tokio::test]
async fn open_tool_call_holds_the_quiet_settle_off() {
    init_env();
    let events = run_and_collect(
        AcpHarness::grok(),
        "scenario:quiet-tool-guard",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    // The "finished" text (streamed after the quiet stretch) must precede
    // the single Done — a premature settle would have flipped that order.
    let finished = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::TextDelta { text } if text == "finished"))
        .expect("post-quiet text must fold into the SAME turn: {events:?}");
    let done = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(
        finished < done,
        "the turn must survive the quiet stretch intact: {events:?}"
    );
}

/// Pi has the same hole as the retired Claude ACP adapter: mid-turn
/// compaction and long reasoning emit no ACP session/update, so the
/// "looks finished" quiet window would forge a Completed on a live
/// prompt. Pi must ignore the blanket settle even with the env knob set.
#[tokio::test]
async fn pi_thinking_silence_never_settles_the_turn() {
    init_env();
    let events = run_and_collect(
        AcpHarness::pi(),
        "scenario:quiet-thinking",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    let finished = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::TextDelta { text } if text == "finished"))
        .unwrap_or_else(|| panic!("post-quiet text must fold into the SAME turn: {events:?}"));
    let done = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(
        finished < done,
        "the turn must survive silent thinking intact: {events:?}"
    );
}

