//! zeron-harness — one interface over coding agents (plus a mock for tests).
//!
//! NATIVE DRIVERS speak each agent's own wire directly: Claude Code over
//! stream-json ([`ClaudeHarness`]), Codex over the app-server JSON-RPC
//! ([`CodexHarness`]), Cursor through a pinned @cursor/sdk shim
//! ([`CursorHarness`]), and opencode over its own HTTP/SSE server protocol
//! ([`OpencodeHarness`] — what the opencode desktop app speaks). The shared
//! [`AcpHarness`] remains ONLY for agents built ground-up on ACP — Devin
//! (`devin acp`), Grok (`grok agent stdio`) and Hermes (`hermes acp`) — plus
//! pi via the community `pi-acp` adapter until a native driver exists.
//! Adapter-mediated ACP for claude/codex/cursor was retired — and opencode's
//! ACP layer with it: the adapters held prompt turns open for background
//! work the CLIs themselves settle eagerly (and opencode's settles on the
//! first uncorrelated idle), manufacturing done-status bugs the native
//! wires don't have (decision record: docs/research/acp.md).

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use zeron_proto::{
    AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    /// A managed adapter install (npm) failed; carries npm's own output so
    /// the cause is diagnosable from the chat error alone.
    #[error("adapter install failed: {0}")]
    Install(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// The run sends questions and awaits answers (blocks the agent, mirrors zeron).
    pub request_input: Box<
        dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
    >,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    fn supports_steering(&self) -> bool;
    fn steering_mode(&self) -> SteeringMode;
    fn reasoning_levels(&self) -> &[ReasoningLevel];
    /// Whether the agent's own CLI is present on this device — the settings
    /// gate for enabling the harness. A filesystem probe, never a spawn.
    /// Defaults to true for harnesses without a CLI to check (mock).
    fn installed(&self) -> bool {
        true
    }
    /// Whether every turn shape — user-prompted AND agent-initiated
    /// (background-subagent wakes) — ends with a deterministic `Done` from
    /// the agent's own wire. Native drivers reading the CLI's terminal frame
    /// directly return true, and the engine retires its quiesce watchdogs
    /// for them; adapter-mediated ACP agents keep the watchdog backstop.
    fn deterministic_turn_end(&self) -> bool {
        false
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    /// Slash commands the agent advertises (ACP `availableCommands`); empty
    /// for harnesses without them. May spawn a short-lived discovery process.
    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(Vec::new())
    }
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

pub mod acp;
pub(crate) mod adapter_install;
pub mod claude;
pub mod codex;
pub mod cursor;
pub(crate) mod jsonrpc;
pub mod mock;
pub mod opencode;
pub mod shell_env;

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        dirs.push(root.join("aliases").join("default").join("bin"));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> =
                entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

/// Add the login shell's PATH to a child process while preserving the PATH of
/// the current process. This lets GUI/service launches find user-installed
/// CLIs such as Homebrew's `gh` without changing the daemon's own environment.
pub fn compose_login_shell_path(cmd: &mut tokio::process::Command) {
    compose_path(cmd, std::iter::empty());
}

/// Compose the child's PATH: the resolved executable's directory first, then
/// our own PATH, then the login-shell PATH snapshot — deduped. npm-shim CLIs
/// are `#!/usr/bin/env node` scripts whose `node` lives beside them in the
/// version manager's bin dir, and the CLIs themselves shell out to tools
/// (git, rg, node) that a GUI/service launch's own PATH may lack.
pub(crate) fn compose_child_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    compose_path(cmd, exe.parent().filter(|d| !d.as_os_str().is_empty()));
}

fn compose_path<'a>(
    cmd: &mut tokio::process::Command,
    executable_dir: impl IntoIterator<Item = &'a std::path::Path>,
) {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for dir in executable_dir {
        paths.push(dir.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(shell_path) = shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(shell_path));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old zeron showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

pub use acp::AcpHarness;
pub use claude::ClaudeHarness;
pub use codex::CodexHarness;
pub use cursor::CursorHarness;
pub use opencode::OpencodeHarness;

// ---------------------------------------------------------------------------
// Child lifecycle (shared by the codex and ACP harnesses)
// ---------------------------------------------------------------------------

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
pub(crate) async fn shutdown_child(
    child: &mut tokio::process::Child,
    kill_grace: std::time::Duration,
) {
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
pub(crate) enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, signal: Signal) {
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
pub(crate) fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}
