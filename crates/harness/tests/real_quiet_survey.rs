//! Production-settings survey across ALL harnesses: does a real multi-tool
//! turn settle exactly once, at the end, and how close do its silent gaps
//! come to the 30s blanket quiet-settle window? Run explicitly:
//!
//!   SURVEY_RUNS=3 cargo test -p zeron-harness --test real_quiet_survey -- --ignored --nocapture
//!
//! No env knob is set here — this binary runs the DEFAULTS the app ships:
//! Claude exempt from the blanket settle, every other adapter on the 30s
//! window. For each installed+authenticated agent CLI it reports, per run:
//! the Done count, content events after the first Done (orphan signature),
//! and the maximum inter-event silent gap — the safety margin against the
//! window. Uninstalled/unauthenticated agents are skipped by name.

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

const POST_DONE_WINDOW: Duration = Duration::from_secs(20);

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

struct ProbeOutcome {
    dones: Vec<(DoneStatus, Option<String>)>,
    after_first_done: usize,
    finished: bool,
    max_gap: Duration,
    started_err: Option<String>,
}

async fn probe_once(harness: AcpHarness) -> ProbeOutcome {
    let (controls, steer_tx, _token) = controls();
    let req = RunRequest {
        prompt: "Use your shell tool to run `echo probe-one`. After you see its output, \
                 run `echo probe-two` as a second separate command. After that, reply \
                 with exactly the word PROBE-DONE."
            .into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let mut stream = match harness.run(req, controls).await {
        Ok(s) => s,
        Err(e) => {
            return ProbeOutcome {
                dones: Vec::new(),
                after_first_done: 0,
                finished: false,
                max_gap: Duration::ZERO,
                started_err: Some(e.to_string()),
            };
        }
    };
    let started = std::time::Instant::now();
    let mut events: Vec<(Duration, AgentEvent)> = Vec::new();
    let mut steer = Some(steer_tx);
    let mut first_done_at: Option<std::time::Instant> = None;
    loop {
        let budget = match first_done_at {
            Some(at) => {
                let left = POST_DONE_WINDOW.saturating_sub(at.elapsed());
                if left.is_zero() && steer.is_some() {
                    steer = None;
                }
                left.max(Duration::from_secs(10))
            }
            None => Duration::from_secs(120),
        };
        match tokio::time::timeout(budget, stream.next()).await {
            Ok(Some(Ok(ev))) => {
                if matches!(ev, AgentEvent::Done { .. }) && first_done_at.is_none() {
                    first_done_at = Some(std::time::Instant::now());
                }
                events.push((started.elapsed(), ev));
            }
            Ok(Some(Err(e))) => {
                events.push((
                    started.elapsed(),
                    AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    },
                ));
                break;
            }
            Ok(None) => break,
            Err(_) => {
                if steer.is_some() {
                    steer = None;
                } else {
                    break;
                }
            }
        }
        if events.len() > 600 {
            break;
        }
    }
    let first_done_idx = events
        .iter()
        .position(|(_, e)| matches!(e, AgentEvent::Done { .. }));
    // Max silent gap while the turn is live: consecutive-event gaps from the
    // first event through the first Done (post-Done observation excluded).
    let live_end = first_done_idx.unwrap_or(events.len().saturating_sub(1));
    let max_gap = events[..=live_end.min(events.len().saturating_sub(1))]
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or(Duration::ZERO);
    let after_first_done = first_done_idx
        .map(|i| {
            events[i + 1..]
                .iter()
                .filter(|(_, e)| {
                    matches!(
                        e,
                        AgentEvent::TextDelta { .. }
                            | AgentEvent::ToolCall { .. }
                            | AgentEvent::ToolResult { .. }
                    )
                })
                .count()
        })
        .unwrap_or(0);
    let text: String = events
        .iter()
        .filter_map(|(_, e)| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    ProbeOutcome {
        dones: events
            .iter()
            .filter_map(|(_, e)| match e {
                AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
                _ => None,
            })
            .collect(),
        after_first_done,
        finished: text.contains("PROBE-DONE"),
        max_gap,
        started_err: None,
    }
}

fn is_auth_or_missing(o: &ProbeOutcome) -> bool {
    let msg = o
        .started_err
        .clone()
        .or_else(|| o.dones.iter().find_map(|(_, e)| e.clone()))
        .unwrap_or_default()
        .to_lowercase();
    msg.contains("auth")
        || msg.contains("login")
        || msg.contains("not installed")
        || msg.contains("not found")
        || msg.contains("no such file")
}

#[tokio::test]
#[ignore = "runs every installed+authenticated agent CLI; costs a few small prompts each"]
async fn real_all_harnesses_quiet_survey() {
    let runs: usize = std::env::var("SURVEY_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let agents: Vec<(&str, fn() -> AcpHarness)> = vec![
        ("devin", AcpHarness::devin),
        ("grok", AcpHarness::grok),
        ("hermes", AcpHarness::hermes),
        ("pi", AcpHarness::pi),
    ];
    let mut failures: Vec<String> = Vec::new();
    for (name, ctor) in agents {
        for i in 1..=runs {
            let o = probe_once(ctor()).await;
            if is_auth_or_missing(&o) {
                println!("[{name}] SKIP — not installed / not authenticated");
                break;
            }
            let clean = o.dones.len() == 1
                && o.dones[0].0 == DoneStatus::Completed
                && o.after_first_done == 0
                && o.finished;
            println!(
                "[{name}] run {i}/{runs}: dones={:?} after_first_done={} finished={} \
                 max_live_gap={:.3}s (window margin {:.1}x) → {}",
                o.dones
                    .iter()
                    .map(|(s, _)| format!("{s:?}"))
                    .collect::<Vec<_>>(),
                o.after_first_done,
                o.finished,
                o.max_gap.as_secs_f64(),
                30.0 / o.max_gap.as_secs_f64().max(0.001),
                if clean { "CLEAN" } else { "VIOLATION" }
            );
            if !clean {
                failures.push(format!(
                    "[{name}] run {i}: dones={:?} after={} finished={}",
                    o.dones, o.after_first_done, o.finished
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
