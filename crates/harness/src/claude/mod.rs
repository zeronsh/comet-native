//! Claude Code harness: spawns the installed `claude` CLI and speaks its
//! stream-json protocol directly (spec: docs/research/harness.md; behavior
//! ported from comet's `packages/harness/src/claude.ts`).
//!
//! - stdout JSONL frames are normalized into [`AgentEvent`]s (init dedupe,
//!   subagent filtering, typed tool decoding, error-code mapping).
//! - The bidirectional control channel is served: `can_use_tool` requests are
//!   auto-allowed, except `AskUserQuestion` which round-trips through
//!   [`RunControls::request_input`] (InputRequested → answers → InputResolved).
//! - Steering: queued [`SteerMessage`]s are written to stdin as user lines at
//!   any time; the CLI applies them at its own step boundary.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends the protocol-level
//!   interrupt control request, then escalates to SIGTERM and SIGKILL.

mod catalog;
mod normalize;
mod wire;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};
use catalog::{apply_ultrathink, static_models, to_effort};
use normalize::Normalizer;
use wire::{ControlRequestFrame, Frame, allow_response, control_response_line};

/// Locate the device's installed Claude Code CLI: `CLAUDE_CODE_EXECUTABLE`,
/// then PATH, then common install locations GUI launches miss (whose PATH the
/// login shell never shaped). Resolved per call — cheap, and PATH may be
/// adopted from the login shell after startup.
fn resolve_claude_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLAUDE_CODE_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".claude").join("local").join("claude"));
        candidates.push(home.join(".local").join("bin").join("claude"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));
    candidates.into_iter().find(|p| p.exists())
}

fn option_is_on(options: &serde_json::Map<String, Value>, key: &str) -> bool {
    match options.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "on" || s == "true",
        _ => false,
    }
}

/// The Claude Code harness. Construct with [`ClaudeHarness::new`]; tests point
/// it at a fake CLI with [`ClaudeHarness::with_executable`].
pub struct ClaudeHarness {
    executable: Option<PathBuf>,
    /// Grace between the interrupt control request and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
}

impl Default for ClaudeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl ClaudeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_claude_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "claude (searched PATH, ~/.claude/local, ~/.local/bin, /opt/homebrew/bin, \
                 /usr/local/bin; set CLAUDE_CODE_EXECUTABLE to override)"
                    .into(),
            )
        })
    }

    fn build_command(&self, exe: &PathBuf, request: &RunRequest) -> Command {
        let mut cmd = Command::new(exe);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            // Route permission prompts to the stdio control channel so
            // `can_use_tool` (and AskUserQuestion in particular) reaches us.
            "--permission-prompt-tool",
            "stdio",
        ]);
        // The 1M context window is selected via a model-id suffix
        // (`sonnet[1m]`), exactly how the CLI itself does it; fast mode and
        // always-on thinking are settings overrides.
        if let Some(model) = &request.model {
            let one_m = request
                .model_options
                .get("contextWindow")
                .and_then(Value::as_str)
                == Some("1m");
            cmd.arg("--model");
            cmd.arg(if one_m {
                format!("{model}[1m]")
            } else {
                model.clone()
            });
        }
        if let Some(effort) = to_effort(request.reasoning, request.model.as_deref()) {
            cmd.args(["--effort", effort]);
        }
        if request.auto_approve {
            cmd.args([
                "--permission-mode",
                "bypassPermissions",
                "--dangerously-skip-permissions",
            ]);
        } else {
            cmd.args(["--permission-mode", "default"]);
        }
        if let Some(resume) = &request.resume {
            cmd.arg(format!("--resume={resume}"));
        }
        let mut settings = serde_json::Map::new();
        if option_is_on(&request.model_options, "fastMode") {
            settings.insert("fastMode".into(), Value::Bool(true));
        }
        if option_is_on(&request.model_options, "thinking") {
            settings.insert("alwaysThinkingEnabled".into(), Value::Bool(true));
        }
        if request.reasoning == Some(ReasoningLevel::Ultracode) {
            settings.insert("ultracode".into(), Value::Bool(true));
        }
        if !settings.is_empty() {
            cmd.arg("--settings");
            cmd.arg(Value::Object(settings).to_string());
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn display_name(&self) -> &str {
        "Claude Code"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }

    /// The curated static catalog (see [`catalog`]); requires an installed CLI
    /// so an absent binary surfaces as [`HarnessError::NotInstalled`] here,
    /// like the TS harness's discovery call.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok(static_models())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = self.build_command(&exe, &request);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdout".into()))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::claude", "stderr: {line}");
                }
            });
        }

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<StdinMsg>();
        tokio::spawn(stdin_writer(stdin, stdin_rx));

        // The initial prompt as the first stdin user line (streaming-input
        // mode). Ultrathink rides every user message — steers included.
        let first = wire::user_message_line(&apply_ultrathink(request.reasoning, &request.prompt));
        let _ = stdin_tx.send(StdinMsg::Line(first));

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            stdout_lines: BufReader::new(stdout).lines(),
            stdin_tx,
            event_tx,
            controls,
            reasoning: request.reasoning,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

enum StdinMsg {
    Line(String),
    /// Close stdin (end of steering input): the CLI finishes the current turn
    /// and exits, which ends the run stream at stdout EOF.
    Close,
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged, matching the TS harness's swallowed-EPIPE behavior.
async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            StdinMsg::Line(line) => {
                let write = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if let Err(e) = write.await {
                    tracing::debug!(target: "comet_harness::claude", "stdin write failed (tolerated): {e}");
                    return;
                }
            }
            StdinMsg::Close => {
                let _ = stdin.shutdown().await;
                return;
            }
        }
    }
}

struct Session {
    child: Child,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin_tx: mpsc::UnboundedSender<StdinMsg>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    reasoning: Option<ReasoningLevel>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

/// The per-run event loop: one task multiplexing stdout frames, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        mut stdout_lines,
        stdin_tx,
        event_tx,
        controls,
        reasoning,
        interrupt_grace,
        kill_grace,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);

    let mut norm = Normalizer::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame = match wire::parse_frame(line) {
                        Ok(frame) => frame,
                        Err(e) => {
                            tracing::debug!(target: "comet_harness::claude", "unparseable frame (skipped): {e}");
                            continue;
                        }
                    };
                    if let Frame::ControlRequest(req) = frame {
                        handle_control_request(req, &request_input, &stdin_tx, &event_tx);
                        continue;
                    }
                    for ev in norm.normalize(frame, interrupted) {
                        let is_done = matches!(ev, AgentEvent::Done { .. });
                        if event_tx.send(Ok(ev)).await.is_err() {
                            break 'main; // consumer gone — reap below
                        }
                        if is_done {
                            any_done = true;
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                        }
                    }
                }
                Ok(None) => break 'main, // stdout EOF: the CLI exited
                Err(e) => {
                    let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let line = wire::user_message_line(&apply_ultrathink(reasoning, &msg.prompt));
                    let _ = stdin_tx.send(StdinMsg::Line(line));
                    // The CLI consumes the queued line at its own step
                    // boundary; rotate the assistant message id so post-steer
                    // output folds into a fresh message.
                    let (prev, next) = norm.rotate_for_steer();
                    let ev = AgentEvent::Steered {
                        assistant_message_id: Some(prev),
                        next_assistant_message_id: Some(next),
                    };
                    if event_tx.send(Ok(ev)).await.is_err() {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed: end the input so the run can finish
                    // after the current turn (mirrors claude.ts steeredInput).
                    steering_open = false;
                    let _ = stdin_tx.send(StdinMsg::Close);
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                let _ = stdin_tx.send(StdinMsg::Line(wire::interrupt_request_line("int_1")));
                // Escalate if the CLI doesn't wind down within the grace
                // periods: SIGTERM (kills bash trees, runs SessionEnd hooks),
                // then SIGKILL. Aborted once the child is reaped.
                if let Some(pid) = child.id() {
                    escalation = Some(tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        send_signal(pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        send_signal(pid, Signal::Kill);
                    }));
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: norm.session_id.clone(),
                }))
                .await;
        } else if !interrupted && !any_done {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("claude exited unexpectedly".into()),
                    session_id: norm.session_id.clone(),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
async fn shutdown_child(child: &mut Child, kill_grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Serve one `can_use_tool` control request. Every tool is auto-approved;
/// `AskUserQuestion` is intercepted — surface the questions, wait for the
/// user's answers (in a subtask so the frame loop keeps flowing), and hand
/// them back keyed by question text, as the tool expects.
fn handle_control_request(
    req: ControlRequestFrame,
    request_input: &Arc<RequestInputFn>,
    stdin_tx: &mpsc::UnboundedSender<StdinMsg>,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) {
    if req.request.subtype != "can_use_tool" {
        tracing::debug!(
            target: "comet_harness::claude",
            "unhandled control_request subtype: {}", req.request.subtype
        );
        return;
    }
    if req.request.tool_name != "AskUserQuestion" {
        let line = control_response_line(&req.request_id, allow_response(req.request.input));
        let _ = stdin_tx.send(StdinMsg::Line(line));
        return;
    }
    let request_input = Arc::clone(request_input);
    let stdin_tx = stdin_tx.clone();
    let event_tx = event_tx.clone();
    tokio::spawn(async move {
        let request_id = req.request_id;
        let input = req.request.input;
        let questions = parse_questions(&input);
        let _ = event_tx
            .send(Ok(AgentEvent::InputRequested {
                request_id: request_id.clone(),
                questions: questions.clone(),
            }))
            .await;
        // A dropped sender (caller went away) degrades to empty answers so the
        // agent is unblocked rather than wedged.
        let answers = (request_input)(questions.clone()).await.unwrap_or_default();
        let updated = updated_input_with_answers(&input, &questions, &answers);
        let line = control_response_line(&request_id, allow_response(updated));
        let _ = stdin_tx.send(StdinMsg::Line(line));
        let _ = event_tx
            .send(Ok(AgentEvent::InputResolved { request_id }))
            .await;
    });
}

/// Parse Claude's `AskUserQuestion` tool input into [`UserInputQuestion`]s
/// (tolerant of `header`/`title`, `question`/`prompt`, string or object
/// options — option descriptions are dropped, the wire type carries labels).
fn parse_questions(input: &Value) -> Vec<UserInputQuestion> {
    let raw = input.get("questions").and_then(Value::as_array);
    raw.map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|q| {
            let field =
                |keys: [&str; 2]| keys.iter().find_map(|k| q.get(*k).and_then(Value::as_str));
            UserInputQuestion {
                id: uuid::Uuid::new_v4().to_string(),
                header: field(["header", "title"]).unwrap_or("Question").into(),
                question: field(["question", "prompt"]).unwrap_or("").into(),
                multi_select: ["multiSelect", "multi_select"]
                    .iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_bool))
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|op| match op {
                        Value::String(s) => s.clone(),
                        other => other
                            .get("label")
                            .or_else(|| other.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Merge the user's answers back into the tool input, keyed by question text
/// (single-select ⇒ a string, multi-select ⇒ an array), as the tool expects.
fn updated_input_with_answers(
    input: &Value,
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> Value {
    let mut updated = match input {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut by_question = serde_json::Map::new();
    for q in questions {
        let labels: Vec<String> = answers
            .iter()
            .find(|a| a.question_id == q.id)
            .map(|a| a.labels.clone())
            .unwrap_or_default();
        let value = if q.multi_select {
            Value::Array(labels.into_iter().map(Value::String).collect())
        } else {
            Value::String(labels.into_iter().next().unwrap_or_default())
        };
        by_question.insert(q.question.clone(), value);
    }
    updated.insert("answers".into(), Value::Object(by_question));
    Value::Object(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_tolerantly() {
        let input = json!({
            "questions": [
                {
                    "header": "Choice",
                    "question": "Pick one",
                    "options": ["A", {"label": "B", "description": "second"}],
                    "multiSelect": false
                },
                { "title": "Alt", "prompt": "Pick many", "multi_select": true }
            ]
        });
        let qs = parse_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header, "Choice");
        assert_eq!(qs[0].options, vec!["A".to_string(), "B".to_string()]);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[1].header, "Alt");
        assert_eq!(qs[1].question, "Pick many");
        assert!(qs[1].multi_select);
    }

    #[test]
    fn answers_key_by_question_text() {
        let input =
            json!({"questions": [{"header": "H", "question": "Pick one", "options": ["A", "B"]}]});
        let qs = parse_questions(&input);
        let answers = vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["B".into()],
        }];
        let updated = updated_input_with_answers(&input, &qs, &answers);
        assert_eq!(updated["answers"]["Pick one"], json!("B"));
        // Original input is preserved alongside the answers.
        assert!(updated["questions"].is_array());
    }
}
