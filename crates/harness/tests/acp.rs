//! AcpHarness integration tests against the fake ACP agent in
//! `tests/fixtures/fake-acp.sh` (no real `grok` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SteeringMode, TodoItem, ToolCall,
    UserInputAnswer,
};

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

fn harness() -> AcpHarness {
    AcpHarness::grok().with_executable(fixture_path())
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
        request_input: Box::new(move |questions| {
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

async fn run_to_end(
    harness: &AcpHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

fn dones(events: &[AgentEvent]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn happy_path_maps_chunks_tools_diffs_plans_and_commands() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;

    // SessionStarted from session/new's id.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { harness, session_id, cwd, .. }
                if *harness == HarnessId::Grok && session_id == "s-1" && cwd == "/tmp"
        )),
        "{events:?}"
    );

    // Initialize-advertised commands surface before the turn.
    let commands: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AvailableCommands { commands } => Some(commands.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(commands.len(), 2, "{events:?}");
    assert_eq!(commands[0][0].name, "compact");
    assert_eq!(commands[0][1].input_hint.as_deref(), Some("the goal"));
    // Mid-run advertisement replaces the list.
    assert_eq!(commands[1][0].name, "deep-research");

    // Chunks; the wrong-session and non-text chunks never surface.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking".into()
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("WRONG"))),
        "{events:?}"
    );

    // Execute tool: pending opens the call, the completed update resolves it
    // with capped multi-line output (newlines preserved verbatim).
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t1".into(),
        call: ToolCall::Exec {
            command: "cargo test -p zeron-harness".into()
        },
    }));
    let exec_output = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error: false,
                output: Some(output),
                ..
            } if id == "t1" => Some(output.clone()),
            _ => None,
        })
        .expect("exec output present");
    assert!(exec_output.starts_with("   Compiling zeron-harness"));
    assert_eq!(exec_output.lines().count(), 6, "{exec_output:?}");

    // Edit tool: single-shot completed call carries the inline diff.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t2".into(),
        call: ToolCall::EditFile {
            path: "/w/src/resolve.rs".into(),
            old_string: None,
            new_string: None,
        },
    }));
    let diff = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                diff: Some(diff),
                ..
            } if id == "t2" => Some(diff.clone()),
            _ => None,
        })
        .expect("edit diff present");
    assert_eq!(diff.path, "/w/src/resolve.rs");
    assert!(
        diff.old_text
            .as_deref()
            .is_some_and(|t| t.contains(".filter(|p| p.exists())")),
        "{diff:?}"
    );
    assert!(diff.new_text.contains("split_paths"), "{diff:?}");

    // Plan → stable todo chip.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "acp-plan".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "read".into(),
                    done: true
                },
                TodoItem {
                    text: "fix".into(),
                    done: false
                },
            ]
        },
    }));

    // usage_update maps to nothing (context gauge, not per-turn tokens).
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Usage { .. })));

    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn config_options_apply_requested_model_and_effort() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:config");
    req.reasoning = Some(zeron_proto::ReasoningLevel::Medium);
    let events = run_to_end(&harness(), req, controls).await;
    // The fixture answers refusal unless BOTH set_config_option calls
    // (model grok-4.5, effort medium) arrived before the prompt.
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "configured".into()
        }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn resumed_first_class_model_is_switched_before_prompt() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:model-api");
    req.resume = Some("existing-grok-session".into());
    let events = run_to_end(&harness(), req, controls).await;
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "model switched".into()
        }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn permission_requests_auto_accept_the_preferred_allow_option() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:permission"), controls).await;
    // The fixture answers refusal unless the harness selected "always".
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "approved".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn steering_extension_injects_mid_turn() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-ext"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first") {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

/// The steering response racing the turn's own end: the injection landed in
/// the dying turn, and the prompt response reached the wire first. The
/// boundary must still be emitted BEFORE the Done — a Steered after Done
/// re-armed the consumer (parked session → Working) with no next turn and no
/// Done ever coming (the stranded-Working / eternal-timer bug).
#[tokio::test]
async fn steer_racing_the_turn_end_never_emits_steered_after_done() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-race"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first") {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("steer landed in the turn: a Steered boundary must exist");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("checked above");
    assert!(
        steered < done,
        "Steered after Done strands the session: {events:?}"
    );
}

#[tokio::test]
async fn rejected_steer_queues_and_delivers_at_the_turn_boundary() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-queue"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first")
                && let Some(steer) = &steer
            {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            // Close the mailbox once the boundary turn streams so the
            // persistent session winds down and the stream ends.
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "boundary") {
                steer = None;
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // First turn completes, then the queued steer becomes the boundary turn.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "boundary".into()
    }));
}

#[tokio::test]
async fn interrupt_sends_session_cancel_and_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");
    assert_eq!(dones(&events), vec![(DoneStatus::Interrupted, None)]);
}

#[tokio::test]
async fn wedged_agent_escalates_to_signals_and_still_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness().with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("escalation reaped the child in time");
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    assert_eq!(dones[0].0, DoneStatus::Interrupted);
}

#[tokio::test]
async fn refusal_maps_to_an_errored_done() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:refusal"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1);
    assert_eq!(dones[0].0, DoneStatus::Errored);
    assert!(dones[0].1.as_deref().unwrap_or("").contains("refused"));
}

#[tokio::test]
async fn resume_loads_the_session_and_drops_replayed_history() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:resumed");
    req.resume = Some("s-loaded".into());
    let events = run_to_end(&harness(), req, controls).await;
    // The 600-update replay is drained without surfacing…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("old reply"))),
        "{events:?}"
    );
    // …the loaded session id sticks, and the live turn still streams.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "s-loaded"
    )));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "back again".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}
#[test]
fn descriptor_surface_matches_registry_expectations() {
    let harness = AcpHarness::grok();
    assert_eq!(harness.id(), HarnessId::Grok);
    assert_eq!(harness.display_name(), "Grok");
    assert!(harness.supports_steering());
    assert_eq!(harness.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        harness.reasoning_levels(),
        &[
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
        ]
    );
}

#[tokio::test]
async fn models_are_discovered_from_the_acp_session() {
    // ACP is the source of truth: the fixture advertises a model config
    // option, so the picker list comes from the wire, not the static catalog.
    let harness = AcpHarness::hermes().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["grok-4-fast", "grok-4.5"], "{models:?}");
    // Unmatched ids inherit the probe session's thought_level ladder.
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
        ],
        "{models:?}"
    );
    assert_eq!(models[0].description.as_deref(), Some("Fast tier"));
    // Cached: a second call returns the same list without respawning.
    let again = harness.models().await.expect("cached");
    assert_eq!(again, models);
}

#[tokio::test]
async fn models_enrich_from_the_static_catalog_on_id_match() {
    // grok's static catalog knows "grok-4.5" — the discovered entry keeps the
    // wire label but inherits the curated description and ladder.
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let grok45 = models
        .iter()
        .find(|m| m.id == "grok-4.5")
        .expect("grok-4.5");
    assert_eq!(
        grok45.description.as_deref(),
        Some("xAI's coding model — 500k context"),
        "{grok45:?}"
    );
}

#[tokio::test]
async fn models_fall_back_to_the_static_catalog_when_the_probe_fails() {
    let harness = AcpHarness::pi().with_executable("/nonexistent/never-a-pi-acp");
    let models = harness.models().await.expect("static fallback");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["default"], "{models:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn hung_handshake_errors_instead_of_spinning_forever() {
    // An agent that consumes stdin and never answers initialize — the
    // "thinking for minutes, then nothing" startup class (issue #93). The
    // run must end with a Done that names the timeout, not hang.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hung-agent.sh");
    // sleep inherits the stdio pipes and holds them open without ever
    // answering — a true wedge, not a crash.
    std::fs::write(&script, "#!/bin/sh\nexec sleep 1000\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let harness = AcpHarness::grok()
        .with_executable(&script)
        .with_handshake_timeout(Duration::from_millis(300));
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness, request("hi"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    let (status, error) = &dones[0];
    assert_eq!(*status, DoneStatus::Errored);
    let error = error.as_deref().unwrap_or_default();
    assert!(
        error.contains("did not complete the ACP handshake"),
        "{error}"
    );
}
#[test]
fn hermes_and_pi_descriptor_surfaces_match_registry_expectations() {
    let devin = AcpHarness::devin();
    assert_eq!(devin.id(), HarnessId::Devin);
    assert_eq!(devin.display_name(), "Devin");
    assert!(devin.supports_steering());
    assert_eq!(devin.steering_mode(), SteeringMode::TurnBoundary);
    assert!(devin.reasoning_levels().is_empty());

    let hermes = AcpHarness::hermes();
    assert_eq!(hermes.id(), HarnessId::Hermes);
    assert_eq!(hermes.display_name(), "Hermes");
    assert!(hermes.supports_steering());
    assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
    assert!(hermes.reasoning_levels().is_empty());

    let pi = AcpHarness::pi();
    assert_eq!(pi.id(), HarnessId::Pi);
    assert_eq!(pi.display_name(), "Pi");
    assert!(pi.supports_steering());
    assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        pi.reasoning_levels(),
        &[
            zeron_proto::ReasoningLevel::Minimal,
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
            zeron_proto::ReasoningLevel::XHigh,
            zeron_proto::ReasoningLevel::Max,
        ]
    );
}

#[tokio::test]
async fn devin_spec_drives_the_shared_acp_wire() {
    let devin = AcpHarness::devin().with_executable(fixture_path());
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&devin, request("scenario:happy"), controls).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { harness, .. } if *harness == HarnessId::Devin
    )));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn prompt_complete_extension_settles_a_hung_prompt_response() {
    // The grok field hang: `_x.ai/session/prompt_complete` fires (echoing
    // the minted _meta.promptId) but the session/prompt RPC never answers.
    let (controls, _steer, _token) = controls();
    let mut stream = harness()
        .run(request("scenario:prompt-complete-hang"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("notification settled the turn despite the hung RPC");
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "pong".into()
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn stale_prompt_complete_never_settles_a_newer_turn() {
    let (controls, _steer, _token) = controls();
    let mut stream = harness()
        .run(request("scenario:prompt-complete-stale"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("real response settled the turn");
    // Exactly one Done, AFTER the real content — the stale/foreign
    // completions (emitted before the 1s pause) must not have settled first,
    // and must not have marked the turn Interrupted.
    let text = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "real answer"))
        .expect("real content precedes the settle");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done");
    assert!(text < done, "{events:?}");
    assert!(matches!(
        &events[done],
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    ));
    // Grok-style `_meta` usage on the response is captured.
    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 9,
        output_tokens: 4
    }));
}

#[tokio::test]
async fn grok_subagent_lifecycle_tails_the_disk_transcript_into_tagged_events() {
    // The child session's chat_history.jsonl, one level under the sessions
    // root exactly like grok's `<root>/<urlencoded-cwd>/<session-id>/` layout
    // (entry shapes captured from a real 1.0.4 run).
    let tmp = tempfile::tempdir().unwrap();
    let child_dir = tmp.path().join("%2Ftmp").join("sub-1");
    std::fs::create_dir_all(&child_dir).unwrap();
    let history = child_dir.join("chat_history.jsonl");
    std::fs::write(
        &history,
        concat!(
            "{\"type\":\"system\",\"content\":\"You are a Grok Build subagent\"}\n",
            "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Count the files.\"}],\"prompt_index\":0}\n",
            "{\"type\":\"reasoning\",\"id\":\"rs-1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Listing the directory.\"}],\"encrypted_content\":\"opaque\",\"status\":\"completed\"}\n",
            "{\"type\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"id\":\"call-1-0\",\"name\":\"run_terminal_command\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}],\"model_id\":\"grok-4.6-build\"}\n",
            "{\"type\":\"tool_result\",\"tool_call_id\":\"call-1-0\",\"content\":\"a.txt\\nb.txt\"}\n",
        ),
    )
    .unwrap();
    // A mid-run append: the tail must pick it up incrementally, before the
    // wire's subagent_finished lands (the fake agent sleeps 1.4s).
    let append_to = history.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(append_to)
            .unwrap();
        writeln!(
            f,
            "{}",
            "{\"type\":\"assistant\",\"content\":\"two files\",\"model_id\":\"grok-4.6-build\"}"
        )
        .unwrap();
    });

    let (controls, _steer, _token) = controls();
    let harness = harness().with_sessions_root(tmp.path());
    let events = run_to_end(&harness, request("scenario:subagent"), controls).await;

    // The spawn chip is named after the task, claude-driver parity.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
                if id == "sp1" && name == "Agent: Count files"
        )),
        "{events:?}"
    );

    // Tagged transcript: every wrapped event attributes to the spawn chip,
    // and the disk entries surfaced in order — reasoning, the typed tool
    // call + result, the mid-run append — then the lifecycle Done.
    let tagged: Vec<&AgentEvent> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } => {
                assert_eq!(parent_tool_use_id, "sp1", "{events:?}");
                Some(event.as_ref())
            }
            _ => None,
        })
        .collect();
    let pos = |pred: &dyn Fn(&AgentEvent) -> bool| tagged.iter().position(|e| pred(e));
    let reasoning = pos(&|e| {
        matches!(e, AgentEvent::ReasoningDelta { text } if text.starts_with("Listing the directory."))
    })
    .expect("reasoning entry tailed");
    let tool = pos(&|e| {
        matches!(
            e,
            AgentEvent::ToolCall { id, call: zeron_proto::ToolCall::Exec { command } }
                if id == "call-1-0" && command == "ls"
        )
    })
    .expect("tool call typed from disk");
    let result = pos(&|e| {
        matches!(
            e,
            AgentEvent::ToolResult { id, is_error: false, output: Some(o), .. }
                if id == "call-1-0" && o.contains("a.txt")
        )
    })
    .expect("tool result tailed");
    let text =
        pos(&|e| matches!(e, AgentEvent::TextDelta { text } if text.starts_with("two files")))
            .expect("mid-run append tailed");
    let done = pos(&|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    })
    .expect("tagged done from subagent_finished");
    assert!(
        reasoning < tool && tool < result && result < text && text < done,
        "{tagged:?}"
    );
    // The nested spawned update (another parent session) bound nothing —
    // every wrapped event attributed to sp1 (the assert in the filter) — and
    // the parent's own turn settled cleanly with its single untagged Done.
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[cfg(unix)]
fn devin_fixture() -> (tempfile::TempDir, AcpHarness) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("devin.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, pathlib, sys, threading, time
root = pathlib.Path(__file__).parent

def emit(frame):
    print(json.dumps(dict(jsonrpc='2.0', **frame)), flush=True)

def config(model):
    return [{'id':'model', 'category':'model', 'type':'select',
             'currentValue':'gpt-old', 'options':[{'value':model, 'name':model}]}]

if sys.argv[1:] == ['models', 'list', '--format', 'json']:
    with (root / 'probes').open('a') as f: f.write('probe\n')
    state = (root / 'state').read_text()
    if state == 'hang': time.sleep(60)
    if state == 'error':
        print('account unavailable', file=sys.stderr)
        sys.exit(1)
    time.sleep(0.1)
    print(json.dumps({'families':[{'variants':[{'model_uid':state, 'label':state}]}]}))
    sys.exit(0)
assert sys.argv[1:] == ['acp'], sys.argv
selected = None

def refresh():
    # An unrelated session must not satisfy the requested-model wait.
    emit({'method':'session/update', 'params':{'sessionId':'other', 'update':{
        'sessionUpdate':'config_option_update', 'configOptions':config('gpt-new')}}})
    time.sleep(0.1)
    (root / 'refreshed').touch()
    emit({'method':'session/update', 'params':{'sessionId':'s-1', 'update':{
        'sessionUpdate':'config_option_update', 'configOptions':config('gpt-new')}}})

for line in sys.stdin:
    req = json.loads(line)
    method = req.get('method')
    result = {}
    if method == 'initialize': result = {'protocolVersion':1, 'agentCapabilities':{}}
    elif method == 'session/new':
        result = {'sessionId':'s-1', 'configOptions':config('gpt-old')}
    elif method == 'session/set_config_option':
        selected = req['params']['value']
        assert (root / 'refreshed').exists(), 'selected before own session refreshed'
        if (root / 'state').read_text() == 'reject':
            emit({'id':req['id'], 'error':{'code':-32602, 'message':'model unavailable'}})
            continue
        assert selected == 'gpt-new', selected
    elif method == 'session/prompt':
        (root / 'prompted').write_text(selected or 'default')
        result = {'stopReason':'end_turn'}
    early = (root / 'state').read_text() == 'early'
    if method == 'session/new' and early: refresh()
    emit({'id':req['id'], 'result':result})
    if method == 'session/new' and not early: threading.Thread(target=refresh, daemon=True).start()
    if method == 'session/prompt': break
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("state"), "gpt-old").unwrap();
    let harness = AcpHarness::devin().with_executable(script);
    (dir, harness)
}

#[cfg(unix)]
#[tokio::test]
async fn devin_models_refresh_between_calls_and_coalesce_overlapping_probes() {
    let (dir, harness) = devin_fixture();
    let (first, overlap) = tokio::join!(harness.models(), harness.models());
    assert_eq!(first.unwrap()[0].id, "gpt-old");
    assert_eq!(overlap.unwrap()[0].id, "gpt-old");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("probes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    std::fs::write(dir.path().join("state"), "gpt-new").unwrap();
    assert_eq!(harness.models().await.unwrap()[0].id, "gpt-new");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("probes"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn devin_discovery_errors_and_timeouts_retry_without_stale_success() {
    let (dir, harness) = devin_fixture();
    let harness = harness.with_model_discovery_timeout(Duration::from_millis(500));
    assert_eq!(harness.models().await.unwrap()[0].id, "gpt-old");
    std::fs::write(dir.path().join("state"), "error").unwrap();
    assert!(
        harness
            .models()
            .await
            .unwrap_err()
            .to_string()
            .contains("account unavailable")
    );
    std::fs::write(dir.path().join("state"), "hang").unwrap();
    assert!(
        harness
            .models()
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    std::fs::write(dir.path().join("state"), "gpt-new").unwrap();
    assert_eq!(harness.models().await.unwrap()[0].id, "gpt-new");
}

#[cfg(unix)]
#[tokio::test]
async fn devin_waits_for_refreshed_variant_before_prompting() {
    let (dir, harness) = devin_fixture();
    let (controls, _steer, _token) = controls();
    let mut req = request("Say hello");
    req.model = Some("gpt-new".into());
    let events = run_to_end(&harness, req, controls).await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("prompted")).unwrap(),
        "gpt-new"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn devin_rejected_model_never_prompts_with_a_different_model() {
    let (dir, harness) = devin_fixture();
    std::fs::write(dir.path().join("state"), "reject").unwrap();
    let (controls, _steer, _token) = controls();
    let mut req = request("Say hello");
    req.model = Some("gpt-new".into());
    let events = run_to_end(&harness, req, controls).await;
    assert!(
        dones(&events)
            .iter()
            .any(|(status, error)| *status == DoneStatus::Errored
                && error
                    .as_deref()
                    .is_some_and(|e| e.contains("model unavailable"))),
        "{events:?}"
    );
    assert!(!dir.path().join("prompted").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn devin_keeps_model_refresh_that_precedes_session_response() {
    let (dir, harness) = devin_fixture();
    std::fs::write(dir.path().join("state"), "early").unwrap();
    let (controls, _steer, _token) = controls();
    let mut req = request("Say hello");
    req.model = Some("gpt-new".into());
    let events = run_to_end(&harness, req, controls).await;
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("prompted")).unwrap(),
        "gpt-new"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn devin_missing_variant_is_bounded_and_never_prompts() {
    let (dir, harness) = devin_fixture();
    let harness = harness.with_handshake_timeout(Duration::from_millis(300));
    let (controls, _steer, _token) = controls();
    let mut req = request("Say hello");
    req.model = Some("gpt-missing".into());
    let events = run_to_end(&harness, req, controls).await;
    assert!(
        dones(&events)
            .iter()
            .any(|(status, _)| *status == DoneStatus::Errored),
        "{events:?}"
    );
    assert!(!dir.path().join("prompted").exists());
}
