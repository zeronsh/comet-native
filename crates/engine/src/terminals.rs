//! Terminals — PTY sessions owned by this device (feature-inventory §3.4; port of
//! comet's `terminals.ts` over `portable-pty`).
//!
//! - `open` spawns the user's login shell in the chat's cwd; `subscribe` replays a
//!   bounded 1MB window (resumable via `afterSeq`) then tails live output, batched
//!   at [`TERMINAL_OUTPUT_BATCH_MS`]; data rides base64 (PTY bytes ≠ UTF-8).
//! - Live shells survive subscriber detach — a detached session is the user's
//!   running process, kept until its tab is explicitly closed or the engine exits.
//!   Only EXITED sessions expire (30min TTL on their inert replay buffers), and
//!   [`MAX_TERMINALS`] bounds leakage from renderers that lost their tab state.
//! - Ownership: M5 is single-user local — every IPC/relay caller is the device
//!   owner, so the per-user owner re-checks from comet's Router land with real
//!   multi-account auth in M6.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use comet_doc::TERMINAL_OUTPUT_BATCH_MS;
use comet_proto::{TerminalEvent, TerminalSession};

use crate::{EngineError, new_id};

const MAX_TERMINALS: usize = 32;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_REPLAY_BYTES: usize = 1024 * 1024;
const EXITED_TTL: Duration = Duration::from_secs(30 * 60);
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

struct LiveTerminal {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    subscribers: Vec<mpsc::UnboundedSender<TerminalEvent>>,
    replay: VecDeque<TerminalEvent>,
    replay_bytes: usize,
    seq: u64,
    last_active_at: std::time::Instant,
    exited: bool,
}

impl LiveTerminal {
    /// Stamp a seq, append to the bounded replay window, and fan out to live
    /// subscribers. On `Exit` the subscriber senders are dropped so every
    /// attached stream ends after delivering the event.
    fn emit(&mut self, event: TerminalEvent) {
        self.last_active_at = std::time::Instant::now();
        let bytes = match &event {
            TerminalEvent::Data { data, .. } => data.len(),
            TerminalEvent::Exit { .. } => 16,
        };
        self.replay.push_back(event.clone());
        self.replay_bytes += bytes;
        while self.replay_bytes > MAX_REPLAY_BYTES && self.replay.len() > 1 {
            if let Some(dropped) = self.replay.pop_front() {
                self.replay_bytes -= match &dropped {
                    TerminalEvent::Data { data, .. } => data.len(),
                    TerminalEvent::Exit { .. } => 16,
                };
            }
        }
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        if matches!(event, TerminalEvent::Exit { .. }) {
            self.exited = true;
            self.subscribers.clear();
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

struct TerminalsInner {
    sessions: Mutex<HashMap<String, Arc<Mutex<LiveTerminal>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct Terminals {
    inner: Arc<TerminalsInner>,
}

impl Default for Terminals {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(2, 500),
        rows: rows.clamp(1, 300),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The user's interactive shell: `$SHELL`, else the platform default.
fn selected_shell() -> String {
    if cfg!(windows) {
        return std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into());
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/bash".into()
            }
        })
}

impl Terminals {
    /// Requires a tokio runtime (spawns the exited-session reaper).
    pub fn new() -> Self {
        let terminals = Self {
            inner: Arc::new(TerminalsInner {
                sessions: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(reaper_task(Arc::downgrade(&terminals.inner)));
        terminals
    }

    /// Open a login shell in `cwd`. The PTY outlives every subscriber; it dies on
    /// [`Self::close`], shell exit + TTL, or engine shutdown.
    pub fn open(&self, cwd: &str, cols: u16, rows: u16) -> Result<TerminalSession, EngineError> {
        self.open_with_shell(cwd, cols, rows, None)
    }

    /// Explicit shell override (tests use `/bin/sh`).
    pub fn open_with_shell(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
    ) -> Result<TerminalSession, EngineError> {
        if lock(&self.inner.sessions).len() >= MAX_TERMINALS {
            return Err(EngineError::Other(format!(
                "Too many open terminals (maximum {MAX_TERMINALS})"
            )));
        }
        if !std::fs::metadata(cwd).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(EngineError::Other(
                "Session working directory is unavailable".into(),
            ));
        }

        let shell = shell.map(str::to_string).unwrap_or_else(selected_shell);
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| shell.clone());

        let pty = native_pty_system();
        let pair = pty
            .openpty(clamp_size(cols, rows))
            .map_err(|e| EngineError::Other(format!("could not open a pty: {e}")))?;
        let mut cmd = CommandBuilder::new(&shell);
        if !cfg!(windows) {
            cmd.arg("-l"); // login shell — the user's real PATH/profile
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Comet");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| EngineError::Other(format!("could not spawn {shell_name}: {e}")))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| EngineError::Other(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| EngineError::Other(format!("pty writer: {e}")))?;

        let id = new_id();
        let session = Arc::new(Mutex::new(LiveTerminal {
            master: pair.master,
            writer,
            killer,
            subscribers: Vec::new(),
            replay: VecDeque::new(),
            replay_bytes: 0,
            seq: 0,
            last_active_at: std::time::Instant::now(),
            exited: false,
        }));
        lock(&self.inner.sessions).insert(id.clone(), session.clone());

        // Raw PTY bytes: blocking reader thread → batcher task (12ms windows).
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || read_pty(reader, raw_tx))
            .map_err(|e| EngineError::Other(format!("pty reader thread: {e}")))?;
        let wait = tokio::task::spawn_blocking(move || child.wait());
        tokio::spawn(pump_output(Arc::downgrade(&session), raw_rx, wait));

        Ok(TerminalSession {
            id,
            cwd: cwd.to_string(),
            shell: shell_name,
        })
    }

    fn session(&self, terminal_id: &str) -> Result<Arc<Mutex<LiveTerminal>>, EngineError> {
        lock(&self.inner.sessions)
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| EngineError::Other("Terminal not found".into()))
    }

    /// Replay (from `after_seq`, bounded 1MB window) then live tail. The stream
    /// ends after `Exit`; detaching (dropping the stream) leaves the PTY running.
    pub fn subscribe(
        &self,
        terminal_id: &str,
        after_seq: Option<u64>,
    ) -> Result<mpsc::UnboundedReceiver<TerminalEvent>, EngineError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        let (tx, rx) = mpsc::unbounded_channel();
        let after = after_seq.unwrap_or(0);
        for event in &session.replay {
            let seq = match event {
                TerminalEvent::Data { seq, .. } | TerminalEvent::Exit { seq, .. } => *seq,
            };
            if seq > after {
                let _ = tx.send(event.clone());
            }
        }
        if !session.exited {
            session.subscribers.push(tx);
        }
        // On an exited session `tx` drops here: the stream ends after the replay.
        Ok(rx)
    }

    /// Write input bytes; `data` is base64 (matching `Data` events), with a plain
    /// UTF-8 fallback for lenient callers.
    pub fn write(&self, terminal_id: &str, data: &str) -> Result<(), EngineError> {
        let bytes = BASE64
            .decode(data)
            .unwrap_or_else(|_| data.as_bytes().to_vec());
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(EngineError::Other("Terminal input is too large".into()));
        }
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        if session.exited {
            return Err(EngineError::Other("Terminal has exited".into()));
        }
        session.last_active_at = std::time::Instant::now();
        session
            .writer
            .write_all(&bytes)
            .and_then(|_| session.writer.flush())
            .map_err(|e| EngineError::Other(format!("Terminal write failed: {e}")))
    }

    pub fn resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<(), EngineError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        if session.exited {
            return Ok(());
        }
        session
            .master
            .resize(clamp_size(cols, rows))
            .map_err(|e| EngineError::Other(format!("Terminal resize failed: {e}")))
    }

    /// Kill the shell (if still running) and drop the session + replay buffer.
    pub fn close(&self, terminal_id: &str) -> Result<(), EngineError> {
        let session = lock(&self.inner.sessions)
            .remove(terminal_id)
            .ok_or_else(|| EngineError::Other("Terminal not found".into()))?;
        dispose(&session, true);
        Ok(())
    }

    /// Engine shutdown: kill every live shell.
    pub fn shutdown(&self) {
        let sessions: Vec<_> = lock(&self.inner.sessions).drain().map(|(_, s)| s).collect();
        for session in sessions {
            dispose(&session, true);
        }
    }
}

fn dispose(session: &Arc<Mutex<LiveTerminal>>, kill: bool) {
    let mut session = lock(session);
    session.subscribers.clear();
    if kill
        && !session.exited
        && let Err(err) = session.killer.kill()
    {
        tracing::debug!(error = %err, "terminal kill failed (already exited?)");
    }
}

/// Blocking PTY reader: forwards raw chunks until EOF. A closed PTY reads as an
/// error on some platforms (EIO on Linux once the shell exits) — both end the loop.
fn read_pty(mut reader: Box<dyn Read + Send>, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Batches raw chunks into `Data` events every [`TERMINAL_OUTPUT_BATCH_MS`], then —
/// once the reader hits EOF (shell gone) — emits the final `Exit` event. Holds only
/// a weak session handle so a closed terminal tears this task down.
async fn pump_output(
    session: Weak<Mutex<LiveTerminal>>,
    mut raw_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    wait: tokio::task::JoinHandle<Result<portable_pty::ExitStatus, std::io::Error>>,
) {
    let batch = Duration::from_millis(TERMINAL_OUTPUT_BATCH_MS);
    let emit = |buffer: Vec<u8>| -> bool {
        let Some(session) = session.upgrade() else {
            return false;
        };
        let mut session = lock(&session);
        let seq = session.next_seq();
        session.emit(TerminalEvent::Data {
            seq,
            data: BASE64.encode(&buffer),
        });
        true
    };
    'outer: while let Some(first) = raw_rx.recv().await {
        let mut buffer = first;
        let deadline = tokio::time::Instant::now() + batch;
        loop {
            match tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(None) => {
                    // Reader gone: flush, then fall through to the exit stamp.
                    emit(buffer);
                    break 'outer;
                }
                Err(_) => break, // batch window elapsed
            }
        }
        if !emit(buffer) {
            return; // terminal closed underneath us
        }
    }
    let exit_code = match wait.await {
        Ok(Ok(status)) => status.exit_code() as i32,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "terminal wait failed");
            -1
        }
        Err(err) => {
            tracing::debug!(error = %err, "terminal wait task failed");
            -1
        }
    };
    if let Some(session) = session.upgrade() {
        let mut session = lock(&session);
        let seq = session.next_seq();
        session.emit(TerminalEvent::Exit {
            seq,
            exit_code,
            signal: None,
        });
    }
}

/// Live shells never expire on idleness — a detached session is the user's running
/// process. Only EXITED sessions are swept after [`EXITED_TTL`]: they're inert
/// replay buffers held so a returning viewer can show the tail + exit status.
async fn reaper_task(inner: Weak<TerminalsInner>) {
    let mut tick = tokio::time::interval(REAPER_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { break };
        let mut sessions = lock(&inner.sessions);
        sessions.retain(|_, session| {
            let session = lock(session);
            !(session.exited && session.last_active_at.elapsed() > EXITED_TTL)
        });
    }
}
