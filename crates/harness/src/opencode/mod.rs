//! Native opencode driver — the same HTTP + SSE protocol the opencode
//! desktop app speaks, replacing the ACP path (`opencode acp`).
//!
//! Why not ACP: opencode's own ACP layer settles a prompt on the FIRST
//! `session.status{idle}` it observes after subscribing, uncorrelated with
//! the submitted turn, and it ignores `session.error` and `session.idle`
//! entirely (`packages/opencode/src/acp/event.ts`, 1.18.x) — the source of
//! "done while still working" and silently-hung turns. It also drops all
//! subagent traffic and thinking never reaches the ACP wire usefully. The
//! desktop app doesn't use ACP; neither do we.
//!
//! Protocol (verified against the 1.18 "v1" server, the one `opencode serve`
//! and the desktop's embedded sidecar expose):
//! - spawn `opencode serve --port <free> --hostname 127.0.0.1` with
//!   `OPENCODE_SERVER_PASSWORD=<uuid>` (HTTP Basic, username `opencode`);
//!   readiness = `GET /global/health`.
//! - one global SSE bus `GET /global/event` carries every session's
//!   `message.updated` / `message.part.updated` / `message.part.delta` /
//!   `session.status` / `session.idle` / `session.error` / `permission.asked`
//!   / `question.asked` — child (subagent) sessions included, token-level.
//! - a turn is `POST /session/{id}/prompt_async` (204, fire-and-forget); the
//!   END of the turn is `session.status{type:"idle"}` for THAT session —
//!   exactly what the desktop's `session_working()` predicate keys on. Busy
//!   is re-asserted at the top of every agent-loop iteration, so idle after
//!   busy is authoritative, not a lull.
//! - reasoning streams as `reasoning` parts (`message.part.delta`
//!   field="text" between the opening and closing `message.part.updated`
//!   snapshots) → [`AgentEvent::ReasoningDelta`], the thinking feed.
//!
//! Failure surfacing (the #169 class): a dying provider is VISIBLE here —
//! `session.status{type:"retry", attempt, message}` streams per attempt.
//! Attempt ≥ [`RETRY_REPORT_ATTEMPT`] surfaces an error chip; attempt ≥
//! [`RETRY_ABORT_ATTEMPT`] aborts the turn instead of retrying forever.
//! A prompt that produces NO session-scoped event within
//! [`default_stall_bound`] (`ZERON_OPENCODE_STALL_MS`, 0 disables) errors
//! out instead of spinning "Working" forever.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, TodoItem, ToolCall, UserInputAnswer, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls, shutdown_child};

/// opencode loads plugins and MCP config before the server answers; cold
/// plugin-heavy starts can take minutes. Shared by chat startup and model
/// discovery (same boot either way).
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_TIMEOUT_ENV: &str = "ZERON_OPENCODE_STARTUP_TIMEOUT_SECS";

/// Health-poll cadence while the server boots.
const HEALTH_POLL: Duration = Duration::from_millis(150);

/// Bound on ordinary (non-SSE) HTTP calls: everything is loopback and the
/// only slow route is a cold /provider catalog. The synchronous per-turn
/// command endpoint deliberately bypasses this (its response can take the
/// whole turn and is ignored anyway).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Bus reconnect: the server is our own child on loopback, so a dropped
/// stream with a live process is transient — retry briefly, then treat the
/// run as dead (transcript integrity is gone once frames are missed).
const BUS_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const BUS_RECONNECT_ATTEMPTS: u32 = 40;

/// Provider-retry surfacing: report at 3 (a chip; the turn keeps trying),
/// abort at 8 (opencode itself would retry forever — #169's silent loop).
const RETRY_REPORT_ATTEMPT: u64 = 3;
const RETRY_ABORT_ATTEMPT: u64 = 8;

/// Default bound on prompt-send → first session-scoped bus event.
const DEFAULT_STALL_BOUND: Duration = Duration::from_secs(60);
const STALL_ENV: &str = "ZERON_OPENCODE_STALL_MS";

/// What a wedged/silent run usually means for opencode.
const STALL_HINT: &str = "The model provider is likely unreachable or rejecting requests. \
     Check the model/provider setup (`opencode auth list`, opencode.json) or the opencode \
     log (~/.local/share/opencode/log).";

fn startup_timeout() -> Duration {
    std::env::var(STARTUP_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT)
}

fn stall_bound() -> Option<Duration> {
    match std::env::var(STALL_ENV) {
        Ok(v) => match v.parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Duration::from_millis(ms)),
            Err(_) => Some(DEFAULT_STALL_BOUND),
        },
        Err(_) => Some(DEFAULT_STALL_BOUND),
    }
}

fn opencode_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".opencode").join("bin").join("opencode"));
        dirs.push(home.join(".local").join("bin").join("opencode"));
        dirs.push(home.join(".npm-global").join("bin").join("opencode"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/opencode"));
    dirs.push(PathBuf::from("/usr/local/bin/opencode"));
    dirs
}

const INSTALL_HINT: &str = "opencode (searched PATH, the login shell's PATH, ~/.opencode/bin, \
     ~/.local/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, and \
     fnm/nvm/volta/pnpm/bun install dirs; install with \
     `curl -fsSL https://opencode.ai/install | bash` or \
     `npm install -g opencode-ai`, then `opencode auth login`; set \
     OPENCODE_EXECUTABLE to override)";

fn resolve_opencode_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OPENCODE_EXECUTABLE") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    crate::acp::find_on_paths("opencode", opencode_install_paths())
}

/// Effort ladder surfaced in the picker; applied per run by picking the
/// first of these variant ids the chosen model actually advertises. Must
/// mirror the registry descriptor.
const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

/// Preference-ordered variant ids for a requested effort (opencode's
/// reasoning variants: models.dev metadata or `variants` in opencode.json).
fn variant_candidates(reasoning: Option<ReasoningLevel>) -> Vec<&'static str> {
    let Some(level) = reasoning else {
        return Vec::new();
    };
    match level {
        ReasoningLevel::Minimal => vec!["minimal", "low"],
        ReasoningLevel::Low => vec!["low", "minimal"],
        ReasoningLevel::Medium => vec!["medium"],
        ReasoningLevel::High => vec!["high"],
        ReasoningLevel::XHigh => vec!["xhigh", "x-high", "high"],
        ReasoningLevel::Max => vec!["max", "xhigh", "high"],
        ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => {
            vec!["ultra", "max", "high"]
        }
    }
}

fn variant_to_level(id: &str) -> Option<ReasoningLevel> {
    match id {
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" | "x-high" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        _ => None,
    }
}

/// A kernel-assigned free localhost port, released for the child to claim.
fn free_localhost_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .ok()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct OpencodeHarness {
    executable: Option<PathBuf>,
    /// Test seam: an already-running server (no spawn, no auth unless given).
    base_url: Option<String>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    startup_timeout: Duration,
    models_cache: tokio::sync::OnceCell<Vec<Model>>,
    commands_cache: tokio::sync::OnceCell<Vec<SlashCommand>>,
    /// Coalesce concurrent picker/title probes: several cold opencode boots
    /// at once are slower than one.
    probe_lock: tokio::sync::Mutex<()>,
}

impl Default for OpencodeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            base_url: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            startup_timeout: startup_timeout(),
            models_cache: tokio::sync::OnceCell::new(),
            commands_cache: tokio::sync::OnceCell::new(),
            probe_lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl OpencodeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Drive an already-running server (tests): no spawn, no basic auth.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = Some(base.into());
        self
    }

    pub fn with_graces(mut self, interrupt: Duration, kill: Duration) -> Self {
        self.interrupt_grace = interrupt;
        self.kill_grace = kill;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(exe) = &self.executable {
            return Ok(exe.clone());
        }
        resolve_opencode_executable().ok_or_else(|| HarnessError::NotInstalled(INSTALL_HINT.into()))
    }

    /// Boot (or attach to) a server for a run/probe. Probes have no chat cwd:
    /// they boot in the user's home, where global provider config lives.
    async fn server(&self, cwd: Option<&str>) -> Result<Server, HarnessError> {
        if let Some(base) = &self.base_url {
            return Ok(Server::attached(base.clone()));
        }
        let exe = self.resolve_executable()?;
        Server::spawn(&exe, cwd, self.startup_timeout).await
    }

    /// One short-lived server answers both discovery calls; each cache keeps
    /// what it got. Also primes the OTHER cache so the picker's models fetch
    /// and the composer's commands fetch share one boot.
    async fn probe_models(&self) -> Result<Vec<Model>, HarnessError> {
        let _guard = self.probe_lock.lock().await;
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        let mut server = self.server(None).await?;
        let result = async {
            let providers: ProviderCatalog = server.get("/provider", None).await?;
            let models = models_from_providers(&providers);
            if models.is_empty() {
                return Err(HarnessError::Protocol(
                    "opencode advertised no models (`opencode auth login` to configure a provider)"
                        .into(),
                ));
            }
            if let Ok(commands) = server.get_json("/command", None).await {
                let _ = self.commands_cache.set(commands_from_wire(&commands));
            }
            Ok(models)
        }
        .await;
        server.shutdown(self.kill_grace).await;
        result
    }

    async fn probe_commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let _guard = self.probe_lock.lock().await;
        if let Some(commands) = self.commands_cache.get() {
            return Ok(commands.clone());
        }
        let mut server = self.server(None).await?;
        let result = server
            .get_json("/command", None)
            .await
            .map(|v| commands_from_wire(&v));
        server.shutdown(self.kill_grace).await;
        result
    }
}

#[async_trait]
impl Harness for OpencodeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Opencode
    }
    fn display_name(&self) -> &str {
        // Must match the registry's lazy descriptor.
        "OpenCode"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Steers queue and deliver as the next prompt when the live turn goes
    /// idle — opencode has no mid-turn injection on this wire.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }
    fn installed(&self) -> bool {
        self.executable.is_some()
            || self.base_url.is_some()
            || resolve_opencode_executable().is_some()
    }
    /// `session.status{idle}` is a real terminal frame per turn: the engine
    /// can retire its quiesce watchdogs.
    fn deterministic_turn_end(&self) -> bool {
        true
    }

    /// Live discovery off `GET /provider` (what the desktop app populates its
    /// picker from), cached on success. Failures surface — the picker retries.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if self.base_url.is_none() {
            self.resolve_executable()?;
        }
        self.models_cache
            .get_or_try_init(|| self.probe_models())
            .await
            .cloned()
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands_cache
            .get_or_try_init(|| self.probe_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let cwd = (!request.cwd.is_empty()).then(|| request.cwd.clone());
        let server = self.server(cwd.as_deref()).await?;
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            server,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            known_commands: self.commands_cache.get().cloned(),
        }));
        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Server process + HTTP plumbing
// ---------------------------------------------------------------------------

struct Server {
    child: Option<Child>,
    base: String,
    /// `Authorization` header value (`Basic <b64>`), when we own the process.
    auth: Option<String>,
    client: reqwest::Client,
    stderr_tail: crate::StderrTail,
}

impl Server {
    fn attached(base: String) -> Self {
        Self {
            child: None,
            base: base.trim_end_matches('/').to_owned(),
            auth: None,
            client: http_client(),
            stderr_tail: crate::StderrTail::default(),
        }
    }

    /// Spawn `opencode serve` on a free loopback port with a per-run Basic
    /// password, and wait for `GET /global/health`.
    async fn spawn(
        exe: &std::path::Path,
        cwd: Option<&str>,
        startup: Duration,
    ) -> Result<Self, HarnessError> {
        let port = free_localhost_port().ok_or_else(|| {
            HarnessError::Protocol("no free localhost port for opencode serve".into())
        })?;
        let password = uuid::Uuid::new_v4().to_string();
        let mut cmd = Command::new(exe);
        cmd.arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .arg("--hostname")
            .arg("127.0.0.1")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_CLIENT", "zeron");
        crate::compose_child_path(&mut cmd, exe);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::opencode", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        use base64::Engine as _;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
        );
        let mut server = Self {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            auth: Some(auth),
            client: http_client(),
            stderr_tail,
        };

        // Readiness: the server binds a few seconds into the process's life
        // (minutes on plugin-heavy cold starts). An exited child fails fast
        // with its stderr tail instead of burning the whole budget.
        let deadline = tokio::time::Instant::now() + startup;
        loop {
            if let Some(child) = server.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(HarnessError::Protocol(crate::crash_message(
                    "opencode serve",
                    Some(status),
                    &server.stderr_tail,
                )));
            }
            match server.get_raw("/global/health").await {
                Ok(resp) if resp.status().is_success() => break,
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                server.shutdown(Duration::from_secs(1)).await;
                return Err(HarnessError::Protocol(format!(
                    "opencode serve did not become healthy within {}s (raise {} if this \
                     machine's plugin load is genuinely slow)",
                    startup.as_secs(),
                    STARTUP_TIMEOUT_ENV,
                )));
            }
            tokio::time::sleep(HEALTH_POLL).await;
        }
        Ok(server)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(auth) = &self.auth {
            req = req.header(reqwest::header::AUTHORIZATION, auth.clone());
        }
        req
    }

    /// Bounded health probe. opencode's boot window ACCEPTS connections but
    /// parks the request until the app is ready — and a request parked early
    /// enough is never answered at all (observed live, 1.18.18), so an
    /// unbounded send() wedges the whole startup. Abandon and re-poll.
    async fn get_raw(&self, path: &str) -> Result<reqwest::Response, reqwest::Error> {
        self.request(reqwest::Method::GET, path)
            .timeout(Duration::from_secs(2))
            .send()
            .await
    }

    /// GET with the session's directory scope (the server's per-request
    /// instance selector; both carriers set, matching the official SDK).
    async fn get_json(&self, path: &str, directory: Option<&str>) -> Result<Value, HarnessError> {
        self.get_response(path, directory)
            .await?
            .json()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode GET {path}: {e}")))
    }

    async fn get<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        directory: Option<&str>,
    ) -> Result<T, HarnessError> {
        let response = self.get_response(path, directory).await?;
        decode_json_response(response)
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode GET {path}: {e}")))
    }

    async fn get_response(
        &self,
        path: &str,
        directory: Option<&str>,
    ) -> Result<reqwest::Response, HarnessError> {
        let mut req = self
            .request(reqwest::Method::GET, path)
            .timeout(CALL_TIMEOUT);
        if let Some(dir) = directory {
            req = req
                .query(&[("directory", dir)])
                .header("x-opencode-directory", encode_directory(dir));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode GET {path}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HarnessError::Protocol(format!(
                "opencode GET {path}: {status} {}",
                truncate_body(&body)
            )));
        }
        Ok(resp)
    }

    async fn post_json(
        &self,
        path: &str,
        directory: Option<&str>,
        body: &Value,
    ) -> Result<Value, HarnessError> {
        let mut req = self
            .request(reqwest::Method::POST, path)
            .timeout(CALL_TIMEOUT)
            .json(body);
        if let Some(dir) = directory {
            req = req
                .query(&[("directory", dir)])
                .header("x-opencode-directory", encode_directory(dir));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode POST {path}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HarnessError::Protocol(format!(
                "opencode POST {path}: {status} {}",
                truncate_body(&body)
            )));
        }
        Ok(resp.json::<Value>().await.unwrap_or(Value::Null))
    }

    async fn shutdown(&mut self, kill_grace: Duration) {
        if let Some(child) = self.child.as_mut() {
            shutdown_child(child, kill_grace).await;
        }
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        // No global timeout: the SSE stream lives for the whole run and
        // sync calls are bounded per call site.
        .build()
        .unwrap_or_default()
}

/// The official SDK sends `x-opencode-directory` percent-encoded.
fn encode_directory(dir: &str) -> String {
    let mut out = String::with_capacity(dir.len());
    for b in dir.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= 300 {
        trimmed.to_owned()
    } else {
        let mut end = 300;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

// ---------------------------------------------------------------------------
// Discovery mapping
// ---------------------------------------------------------------------------

/// `GET /provider` → picker models: `{providerID}/{modelID}` ids, effort
/// ladder from the model's reasoning variants.
///
/// Only retain what the picker and run setup consume. `/provider` includes
/// the entire models.dev catalog, with large nested capabilities/config maps.
/// Building and cloning a Value for it measured a 112 MB engine heap peak.
/// Serde skips unknown fields and variant bodies without allocating trees.
#[derive(Debug, Default, serde::Deserialize)]
struct ProviderCatalog {
    all: Option<Vec<Provider>>,
    connected: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct Provider {
    id: Option<String>,
    name: Option<String>,
    models: Option<std::collections::BTreeMap<String, ProviderModel>>,
}

#[derive(Debug, serde::Deserialize)]
struct ProviderModel {
    name: Option<String>,
    variants: Option<std::collections::BTreeMap<String, serde::de::IgnoredAny>>,
}

/// Decode large catalogs without collecting a second, full HTTP body first.
/// The blocking parser reads through a 64KiB buffer; network reads stay on
/// Tokio. Dropping the caller also interrupts an outstanding body read.
async fn decode_json_response<T: serde::de::DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
) -> Result<T, String> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let stream = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .take_until(cancel.cancelled_owned());
    let reader = tokio_util::io::StreamReader::new(Box::pin(stream));
    let reader = tokio_util::io::SyncIoBridge::new(reader);
    tokio::task::spawn_blocking(move || {
        serde_json::from_reader(std::io::BufReader::with_capacity(64 * 1024, reader))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Filtered to CONNECTED providers: `all` is the entire models.dev catalog
/// (measured live: 194 providers / 7,203 models, nearly all needing an API
/// key the user hasn't set) — offering it verbatim made the picker slow to
/// open, slow to scroll, and full of models every run of which fails with
/// "Model not found" (field report, v0.2.21). `connected` names exactly
/// the usable set (credentialed + config-declared + the anonymous Zen
/// tier). An absent/empty `connected` (older server) falls back to `all`.
fn models_from_providers(providers: &ProviderCatalog) -> Vec<Model> {
    let connected: std::collections::HashSet<&str> = providers
        .connected
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut out = Vec::new();
    for provider in providers.all.iter().flatten() {
        let Some(provider_id) = provider.id.as_deref() else {
            continue;
        };
        if !connected.is_empty() && !connected.contains(provider_id) {
            continue;
        }
        let provider_name = provider.name.as_deref().unwrap_or(provider_id);
        let Some(models) = &provider.models else {
            continue;
        };
        let mut provider_models: Vec<Model> = models
            .iter()
            .map(|(model_id, model)| {
                let label = model.name.as_deref().unwrap_or(model_id).to_owned();
                let mut levels: Vec<ReasoningLevel> = model
                    .variants
                    .as_ref()
                    .map(|variants| {
                        variants
                            .keys()
                            .filter_map(|k| variant_to_level(k))
                            .collect()
                    })
                    .unwrap_or_default();
                levels.sort();
                levels.dedup();
                Model {
                    id: format!("{provider_id}/{model_id}"),
                    label,
                    description: Some(provider_name.to_owned()),
                    reasoning_levels: levels,
                    options: Vec::new(),
                }
            })
            .collect();
        provider_models.sort_by(|a, b| a.label.cmp(&b.label));
        out.extend(provider_models);
    }
    out
}

fn commands_from_wire(commands: &Value) -> Vec<SlashCommand> {
    commands
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?;
                    Some(SlashCommand {
                        name: name.to_owned(),
                        description: c
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input_hint: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    server: Server,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    known_commands: Option<Vec<SlashCommand>>,
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// What the bus reader hands the session loop.
enum BusMsg {
    /// A stream (re)connected and delivered its first frame. The FIRST one
    /// gates the initial prompt: the v1 bus has NO replay, and a
    /// fast-failing turn (bad model id errors in ~200ms) can emit
    /// busy → session.error → idle before a late subscription exists —
    /// observed live (1.18.21), leaving only the stall watchdog. Later ones
    /// mean a reconnect gap that may have swallowed our idle: the loop
    /// re-syncs from `GET /session/status`.
    Connected,
    Event(Value),
    /// The stream is gone past the reconnect budget (or the reader saw the
    /// consumer close).
    Disconnected,
}

/// Per-part streaming state (dedup between full-part snapshots and deltas).
#[derive(Default)]
struct PartState {
    /// "text" | "reasoning" | "tool".
    kind: String,
    /// Bytes of part text already emitted (snapshots resend the full text).
    emitted: usize,
    tool_started: bool,
    tool_done: bool,
}

/// Streaming state for one opencode session's feed (ours or a child's).
#[derive(Default)]
struct SessionFeed {
    /// messageID → is-assistant (user prompt echoes must not render).
    assistant_messages: HashMap<String, bool>,
    /// Parts whose message ROLE isn't known yet, replayed when it lands.
    pending_parts: Vec<Value>,
    parts: HashMap<String, PartState>,
}

/// A spawned subagent (a `task` tool call on OUR session).
struct ChildRun {
    /// The parent-feed tool part id its traffic is tagged with.
    parent_tool_use_id: String,
    feed: SessionFeed,
    /// Chip settled (tagged Done sent); late traffic drops.
    done: bool,
}

/// A `task` chip awaiting its child session id.
struct PendingSpawn {
    tool_part_id: String,
    description: String,
}

struct TurnState {
    /// A prompt is in flight (busy expected/observed; idle settles it).
    active: bool,
    /// Bus events about our session seen since the prompt was posted.
    saw_activity: bool,
    /// Renderable content (text/reasoning/tool) seen this turn.
    saw_content: bool,
    /// Terminal error to fold into Done.
    error: Option<String>,
    /// Provider-retry chip already surfaced this turn.
    retry_reported: bool,
    /// This turn was aborted because the provider retry loop hit the cap.
    aborted_for_retry: bool,
    /// Deadline for the first session-scoped event after the prompt.
    stall_deadline: Option<tokio::time::Instant>,
}

impl TurnState {
    fn begin(stall: Option<Duration>) -> Self {
        Self {
            active: true,
            saw_activity: false,
            saw_content: false,
            error: None,
            retry_reported: false,
            aborted_for_retry: false,
            stall_deadline: stall.map(|d| tokio::time::Instant::now() + d),
        }
    }

    fn note_activity(&mut self) {
        self.saw_activity = true;
        self.stall_deadline = None;
    }
}

async fn run_session(session: Session) {
    let Session {
        mut server,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        known_commands,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);
    let directory = (!request.cwd.is_empty()).then(|| request.cwd.clone());
    let dir = directory.as_deref();

    // ---- session create/resume -------------------------------------------
    let setup = async {
        let session_id = match &request.resume {
            Some(resume) => {
                // Sessions are durable server-side: resume = reuse the id.
                match server.get_json(&format!("/session/{resume}"), dir).await {
                    Ok(info) => info
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(resume)
                        .to_owned(),
                    Err(e) => {
                        tracing::debug!(
                            target: "zeron_harness::opencode",
                            "session resume failed (starting fresh): {e}"
                        );
                        create_session(&server, dir).await?
                    }
                }
            }
            None => create_session(&server, dir).await?,
        };

        // Provider catalog: resolves the model's advertised reasoning
        // variants so the requested effort only rides models that have it.
        let providers = server
            .get::<ProviderCatalog>("/provider", dir)
            .await
            .unwrap_or_default();
        Ok::<(String, ProviderCatalog), HarnessError>((session_id, providers))
    };
    let (session_id, providers) = tokio::select! {
        res = setup => match res {
            Ok(v) => v,
            Err(e) => {
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e.to_string()),
                    session_id: None,
                }).await;
                server.shutdown(kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = send(&event_tx, AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            }).await;
            server.shutdown(kill_grace).await;
            return;
        }
    };

    let model = request
        .model
        .as_deref()
        .and_then(|m| m.split_once('/'))
        .map(|(provider, model)| (provider.to_owned(), model.to_owned()));
    let variant = model.as_ref().and_then(|(provider, model_id)| {
        pick_variant(&providers, provider, model_id, request.reasoning)
    });
    drop(providers);

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Opencode,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        server.shutdown(kill_grace).await;
        return;
    }

    // Advertise slash commands (composer popup); a warm cache skips the call.
    let commands = match known_commands {
        Some(commands) => commands,
        None => server
            .get_json("/command", dir)
            .await
            .map(|v| commands_from_wire(&v))
            .unwrap_or_default(),
    };
    if !commands.is_empty()
        && !send(
            &event_tx,
            AgentEvent::AvailableCommands {
                commands: commands.clone(),
            },
        )
        .await
    {
        server.shutdown(kill_grace).await;
        return;
    }

    // ---- SSE bus ----------------------------------------------------------
    let (bus_tx, mut bus_rx) = mpsc::channel::<BusMsg>(256);
    let bus_handle = tokio::spawn(bus_task(server.base.clone(), server.auth.clone(), bus_tx));

    // ---- first prompt -----------------------------------------------------
    // The bus has no replay: wait for the subscription to be LIVE before
    // prompting, or a fast-failing turn's whole lifecycle can slip into the
    // gap (observed live: busy → error → idle inside ~200ms). Bounded — the
    // stall watchdog still guards a bus that never comes up.
    let connect_wait = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match bus_rx.recv().await {
                Some(BusMsg::Connected) | None => return,
                // Nothing else can arrive before Connected; drop defensively.
                Some(_) => {}
            }
        }
    })
    .await;
    if connect_wait.is_err() {
        tracing::debug!(
            target: "zeron_harness::opencode",
            "event bus not connected within 15s; prompting anyway"
        );
    }
    let stall = stall_bound();
    let first_body = prompt_body(
        &request.prompt,
        &model,
        variant.as_deref(),
        &request.attachments,
    );
    if let Err(e) = post_prompt(
        &server,
        &session_id,
        dir,
        &commands,
        &request.prompt,
        first_body,
    )
    .await
    {
        let _ = send(
            &event_tx,
            AgentEvent::Error {
                message: e.to_string(),
            },
        )
        .await;
        let _ = send(
            &event_tx,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some(e.to_string()),
                session_id: Some(session_id.clone()),
            },
        )
        .await;
        bus_handle.abort();
        server.shutdown(kill_grace).await;
        return;
    }
    let mut turn = TurnState::begin(stall);

    // ---- main loop --------------------------------------------------------
    let mut main_feed = SessionFeed::default();
    let mut children: HashMap<String, ChildRun> = HashMap::new();
    let mut pending_spawns: VecDeque<PendingSpawn> = VecDeque::new();
    // Child sessions created before their spawn chip was seen (id → title).
    let mut unbound_children: HashMap<String, String> = HashMap::new();
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupt_requested = false;
    let mut pending_usage: Option<AgentEvent> = None;
    let mut done_sent = false;

    // Post-abort grace: the abort endpoint promised an idle; if it never
    // arrives the run hard-stops. Unlike the stall bound this is NOT
    // disarmed by activity — only idle ends an abort.
    let mut abort_deadline: Option<tokio::time::Instant> = None;

    // Idle settlement, shared by the idle bus events and the post-reconnect
    // status re-sync (a macro so `break`/`continue` act on the caller's
    // loop): emit held usage, then Interrupted / next queued steer /
    // AssistantMessageCompleted + Done.
    macro_rules! settle_idle {
        ($label:lifetime) => {{
            if !turn.active {
                continue $label;
            }
            turn.active = false;
            if let Some(usage) = pending_usage.take()
                && !send(&event_tx, usage).await
            {
                break $label;
            }
            if interrupt_requested {
                settle_children(&mut children, &event_tx, DoneStatus::Interrupted).await;
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                }).await;
                done_sent = true;
                break $label;
            }
            if let Some(steer) = queued_steers.pop_front() {
                let (prev, next) = rotate(&mut assistant_message_id);
                if !send(&event_tx, AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                }).await {
                    break $label;
                }
                let body = prompt_body(&steer, &model, variant.as_deref(), &[]);
                match post_prompt(&server, &session_id, dir, &commands, &steer, body).await {
                    Ok(()) => {
                        turn = TurnState::begin(stall);
                        continue $label;
                    }
                    Err(e) => {
                        let _ = send(&event_tx, AgentEvent::Error {
                            message: e.to_string(),
                        }).await;
                        // Fall through to Done below.
                    }
                }
            }
            let (prev, _next) = rotate(&mut assistant_message_id);
            if !send(&event_tx, AgentEvent::AssistantMessageCompleted {
                assistant_message_id: prev,
            }).await {
                break $label;
            }
            let errored = turn.aborted_for_retry
                || (turn.error.is_some() && !turn.saw_content);
            let _ = send(&event_tx, AgentEvent::Done {
                status: if errored {
                    DoneStatus::Errored
                } else {
                    DoneStatus::Completed
                },
                result: None,
                error: if errored { turn.error.clone() } else { None },
                session_id: Some(session_id.clone()),
            }).await;
            done_sent = true;
            break $label;
        }};
    }

    'main: loop {
        // The stall watchdog only arms while a turn awaits its first sign of
        // life; a running tool's silence never trips it (events already
        // proved the turn alive and disarmed it). Computed by value each
        // iteration so the future doesn't hold a borrow of `turn`.
        let deadline = abort_deadline.or_else(|| {
            (turn.active && !turn.saw_activity)
                .then_some(turn.stall_deadline)
                .flatten()
        });
        let stall_sleep = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;

            _ = interrupt.cancelled(), if !interrupt_requested => {
                interrupt_requested = true;
                if turn.active {
                    let path = format!("/session/{session_id}/abort");
                    let abort = tokio::time::timeout(
                        Duration::from_secs(5),
                        server.post_json(&path, dir, &Value::Null),
                    )
                    .await;
                    if !matches!(abort, Ok(Ok(_))) {
                        // The abort endpoint failing means the server itself
                        // is wedged: settle now and tear down hard.
                        settle_children(&mut children, &event_tx, DoneStatus::Interrupted).await;
                        let _ = send(&event_tx, AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            result: None,
                            error: None,
                            session_id: Some(session_id.clone()),
                        }).await;
                        done_sent = true;
                        break 'main;
                    }
                    // Abort emits session.status{idle} (with or without a
                    // live runner) — the settle path below finishes up. The
                    // grace guards a server that never delivers it.
                    abort_deadline = Some(tokio::time::Instant::now() + interrupt_grace);
                } else {
                    let _ = send(&event_tx, AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    }).await;
                    done_sent = true;
                    break 'main;
                }
            }

            steer = steering.recv(), if steering_open => {
                match steer {
                    Some(steer) => {
                        if turn.active {
                            queued_steers.push_back(steer.prompt);
                        } else {
                            // Between turns (shouldn't happen — the engine
                            // steers live runs — but deliver, don't drop).
                            let body = prompt_body(&steer.prompt, &model, variant.as_deref(), &[]);
                            let (prev, next) = rotate(&mut assistant_message_id);
                            let _ = send(&event_tx, AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            }).await;
                            if post_prompt(&server, &session_id, dir, &commands, &steer.prompt, body).await.is_ok() {
                                turn = TurnState::begin(stall);
                            }
                        }
                    }
                    None => steering_open = false,
                }
            }

            _ = stall_sleep => {
                if abort_deadline.is_some() {
                    // Abort acknowledged nothing within the grace: hard stop.
                    settle_children(&mut children, &event_tx, DoneStatus::Interrupted).await;
                    let _ = send(&event_tx, AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    }).await;
                    done_sent = true;
                    break 'main;
                }
                let message = format!(
                    "opencode made no progress for {}s after the prompt. {STALL_HINT}",
                    stall.unwrap_or(DEFAULT_STALL_BOUND).as_secs()
                );
                let _ = send(&event_tx, AgentEvent::Error { message: message.clone() }).await;
                let path = format!("/session/{session_id}/abort");
                let _ = server.post_json(&path, dir, &Value::Null).await;
                settle_children(&mut children, &event_tx, DoneStatus::Interrupted).await;
                let _ = send(&event_tx, AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: Some(session_id.clone()),
                }).await;
                done_sent = true;
                break 'main;
            }

            msg = bus_rx.recv() => {
                let Some(msg) = msg else { break 'main };
                match msg {
                    BusMsg::Connected => {
                        // A RECONNECT mid-turn may have swallowed our idle
                        // (no replay): re-sync from the server's own status
                        // map — absent means idle.
                        if turn.active {
                            let status = server.get_json("/session/status", dir).await;
                            let busy = match &status {
                                Ok(map) => map.get(&session_id).is_some(),
                                // Can't tell: leave the turn running; the
                                // next disconnect or event decides.
                                Err(_) => true,
                            };
                            if !busy {
                                settle_idle!('main);
                            }
                        }
                    }
                    BusMsg::Disconnected => {
                        let crashed = match server.child.as_mut() {
                            Some(child) => child.try_wait().ok().flatten(),
                            None => None,
                        };
                        let message = crate::crash_message(
                            "opencode serve",
                            crashed,
                            &server.stderr_tail,
                        );
                        if turn.active {
                            let _ = send(&event_tx, AgentEvent::Error { message: message.clone() }).await;
                        }
                        settle_children(&mut children, &event_tx, DoneStatus::Interrupted).await;
                        let _ = send(&event_tx, AgentEvent::Done {
                            status: if interrupt_requested {
                                DoneStatus::Interrupted
                            } else {
                                DoneStatus::Errored
                            },
                            result: None,
                            error: (!interrupt_requested).then_some(message),
                            session_id: Some(session_id.clone()),
                        }).await;
                        done_sent = true;
                        break 'main;
                    }
                    BusMsg::Event(event) => {
                        let outcome = handle_bus_event(BusCtx {
                            event: &event,
                            session_id: &session_id,
                            server: &server,
                            dir,
                            event_tx: &event_tx,
                            request_input: &request_input,
                            main_feed: &mut main_feed,
                            children: &mut children,
                            pending_spawns: &mut pending_spawns,
                            unbound_children: &mut unbound_children,
                            turn: &mut turn,
                            pending_usage: &mut pending_usage,
                        }).await;
                        match outcome {
                            BusOutcome::Continue => {}
                            BusOutcome::ConsumerGone => break 'main,
                            BusOutcome::TurnIdle => settle_idle!('main),
                        }
                    }
                }
            }
        }
    }

    if !done_sent {
        // Consumer went away (stream dropped): nothing to report to.
        tracing::debug!(target: "zeron_harness::opencode", "run loop ended without settling");
    }
    bus_handle.abort();
    server.shutdown(kill_grace).await;
}

async fn create_session(server: &Server, dir: Option<&str>) -> Result<String, HarnessError> {
    let created = server.post_json("/session", dir, &json!({})).await?;
    created
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| HarnessError::Protocol("opencode session create returned no id".into()))
}

/// The requested effort as a variant id the model actually advertises.
fn pick_variant(
    providers: &ProviderCatalog,
    provider_id: &str,
    model_id: &str,
    reasoning: Option<ReasoningLevel>,
) -> Option<String> {
    let candidates = variant_candidates(reasoning);
    if candidates.is_empty() {
        return None;
    }
    let variants = providers
        .all
        .as_ref()?
        .iter()
        .find(|p| p.id.as_deref() == Some(provider_id))?
        .models
        .as_ref()?
        .get(model_id)?
        .variants
        .as_ref()?;
    candidates
        .into_iter()
        .find(|c| variants.contains_key(*c))
        .map(str::to_owned)
}

/// Build a `prompt_async` body: text part + attachment file parts.
fn prompt_body(
    prompt: &str,
    model: &Option<(String, String)>,
    variant: Option<&str>,
    attachments: &[String],
) -> Value {
    let mut parts = vec![json!({ "type": "text", "text": prompt })];
    for path in attachments {
        parts.push(json!({
            "type": "file",
            "mime": mime_for(path),
            "filename": std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "url": format!("file://{path}"),
        }));
    }
    let mut body = serde_json::Map::new();
    body.insert("parts".into(), Value::Array(parts));
    if let Some((provider, model)) = model {
        body.insert(
            "model".into(),
            json!({ "providerID": provider, "modelID": model }),
        );
    }
    if let Some(variant) = variant {
        body.insert("variant".into(), Value::String(variant.to_owned()));
    }
    Value::Object(body)
}

fn mime_for(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Send a turn: a leading `/command` known to the agent routes through the
/// command endpoint (the desktop parity — the server does NOT parse slash
/// text out of an ordinary prompt); everything else is `prompt_async`.
/// Both are fire-and-forget for the loop: the command endpoint is
/// synchronous on the wire, so it rides a detached task and the bus
/// delivers the actual turn.
async fn post_prompt(
    server: &Server,
    session_id: &str,
    dir: Option<&str>,
    commands: &[SlashCommand],
    prompt: &str,
    body: Value,
) -> Result<(), HarnessError> {
    if let Some(rest) = prompt.strip_prefix('/') {
        let mut split = rest.splitn(2, char::is_whitespace);
        let name = split.next().unwrap_or_default();
        let arguments = split.next().unwrap_or_default().trim().to_owned();
        if !name.is_empty() && commands.iter().any(|c| c.name == name) {
            let path = format!("/session/{session_id}/command");
            let cmd_body = json!({ "command": name, "arguments": arguments });
            let server_base = server.base.clone();
            let auth = server.auth.clone();
            let dir_owned = dir.map(str::to_owned);
            let path_owned = path.clone();
            tokio::spawn(async move {
                let server = Server {
                    child: None,
                    base: server_base,
                    auth,
                    client: http_client(),
                    stderr_tail: crate::StderrTail::default(),
                };
                // The command endpoint blocks for the whole turn; the bus
                // carries the real events, so this response is ignored —
                // but it must not be cut off mid-turn by CALL_TIMEOUT.
                let mut req = server
                    .request(reqwest::Method::POST, &path_owned)
                    .json(&cmd_body);
                if let Some(dir) = dir_owned.as_deref() {
                    req = req
                        .query(&[("directory", dir)])
                        .header("x-opencode-directory", encode_directory(dir));
                }
                if let Err(e) = req.send().await {
                    tracing::debug!(
                        target: "zeron_harness::opencode",
                        "command turn failed: {e}"
                    );
                }
            });
            return Ok(());
        }
    }
    let path = format!("/session/{session_id}/prompt_async");
    server.post_json(&path, dir, &body).await.map(|_| ())
}

// ---------------------------------------------------------------------------
// Bus event handling
// ---------------------------------------------------------------------------

enum BusOutcome {
    Continue,
    /// Our session's turn reached idle.
    TurnIdle,
    ConsumerGone,
}

type RequestInput = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

struct BusCtx<'a> {
    event: &'a Value,
    session_id: &'a str,
    server: &'a Server,
    dir: Option<&'a str>,
    event_tx: &'a mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request_input: &'a Arc<RequestInput>,
    main_feed: &'a mut SessionFeed,
    children: &'a mut HashMap<String, ChildRun>,
    pending_spawns: &'a mut VecDeque<PendingSpawn>,
    unbound_children: &'a mut HashMap<String, String>,
    turn: &'a mut TurnState,
    pending_usage: &'a mut Option<AgentEvent>,
}

/// Wrap an event as subagent-attributed traffic.
fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

async fn settle_children(
    children: &mut HashMap<String, ChildRun>,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    status: DoneStatus,
) {
    for child in children.values_mut() {
        if child.done {
            continue;
        }
        child.done = true;
        let _ = event_tx
            .send(Ok(tag(
                &child.parent_tool_use_id,
                AgentEvent::Done {
                    status,
                    result: None,
                    error: None,
                    session_id: None,
                },
            )))
            .await;
    }
}

/// Route one `/global/event` payload. The envelope is
/// `{directory, payload: {type, properties}}`; `sync` mirror frames drop.
async fn handle_bus_event(ctx: BusCtx<'_>) -> BusOutcome {
    let BusCtx {
        event,
        session_id,
        server,
        dir,
        event_tx,
        request_input,
        main_feed,
        children,
        pending_spawns,
        unbound_children,
        turn,
        pending_usage,
    } = ctx;
    // Envelope styles: /global/event wraps ({payload: {...}}); a bare
    // /event feed (tests) delivers the payload directly.
    let payload = event.get("payload").unwrap_or(event);
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "sync" || kind.is_empty() {
        return BusOutcome::Continue;
    }
    let props = payload.get("properties").unwrap_or(&Value::Null);
    let event_session = props
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            props
                .get("info")
                .and_then(|i| i.get("sessionID").or_else(|| i.get("id")))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            props
                .get("part")
                .and_then(|p| p.get("sessionID"))
                .and_then(Value::as_str)
        });

    let is_ours = event_session == Some(session_id);
    if is_ours && turn.active {
        turn.note_activity();
    }

    match kind {
        "session.status" if is_ours => {
            let status = props.get("status").unwrap_or(&Value::Null);
            match status.get("type").and_then(Value::as_str) {
                Some("idle") => return BusOutcome::TurnIdle,
                Some("retry") => {
                    let attempt = status.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                    let message = status
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider error");
                    if attempt >= RETRY_ABORT_ATTEMPT && !turn.aborted_for_retry {
                        turn.aborted_for_retry = true;
                        turn.error = Some(format!(
                            "the provider kept failing after {attempt} attempts: {message}"
                        ));
                        let msg = format!(
                            "Giving up after {attempt} provider retries: {message}. {STALL_HINT}"
                        );
                        if !send(event_tx, AgentEvent::Error { message: msg }).await {
                            return BusOutcome::ConsumerGone;
                        }
                        let path = format!("/session/{session_id}/abort");
                        let _ = server.post_json(&path, dir, &Value::Null).await;
                    } else if attempt >= RETRY_REPORT_ATTEMPT && !turn.retry_reported {
                        turn.retry_reported = true;
                        let msg = format!(
                            "The provider is failing and opencode is retrying (attempt \
                             {attempt}): {message}"
                        );
                        if !send(event_tx, AgentEvent::Error { message: msg }).await {
                            return BusOutcome::ConsumerGone;
                        }
                    }
                }
                _ => {}
            }
            BusOutcome::Continue
        }
        "session.idle" if is_ours => BusOutcome::TurnIdle,
        "session.error" => {
            // Errors are session-scoped but a missing id still concerns us
            // (global provider failures).
            if event_session.is_some() && !is_ours {
                return BusOutcome::Continue;
            }
            let error = props.get("error").unwrap_or(&Value::Null);
            let name = error.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "MessageAbortedError" {
                // The abort echo of an interrupt — not an error chip.
                return BusOutcome::Continue;
            }
            let message = error
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if name.is_empty() {
                        "opencode reported an error".to_owned()
                    } else {
                        name.to_owned()
                    }
                });
            // opencode emits the same failure twice (once bare, once wrapped
            // with an exception-name/stack prefix) — one chip per distinct
            // failure: dedupe when either first line contains the other.
            let first = |m: &str| m.lines().next().unwrap_or(m).trim().to_owned();
            let line = first(&message);
            let duplicate = turn.error.as_deref().is_some_and(|prev| {
                let prev = first(prev);
                !line.is_empty() && (prev.contains(&line) || line.contains(&prev))
            });
            turn.error = Some(message.clone());
            if !duplicate && !send(event_tx, AgentEvent::Error { message }).await {
                return BusOutcome::ConsumerGone;
            }
            BusOutcome::Continue
        }
        "session.created" => {
            let info = props.get("info").unwrap_or(&Value::Null);
            // Only DIRECT children bind; a grandchild's parentID is the
            // child's session and renders inside the child's own doc.
            if info.get("parentID").and_then(Value::as_str) != Some(session_id) {
                return BusOutcome::Continue;
            }
            let Some(child_id) = info.get("id").and_then(Value::as_str) else {
                return BusOutcome::Continue;
            };
            if children.contains_key(child_id) {
                return BusOutcome::Continue;
            }
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !bind_child(children, pending_spawns, child_id, title) {
                unbound_children.insert(child_id.to_owned(), title.to_owned());
            }
            BusOutcome::Continue
        }
        "message.updated" => {
            let info = props.get("info").unwrap_or(&Value::Null);
            let (Some(session), Some(message), Some(role)) = (
                info.get("sessionID").and_then(Value::as_str),
                info.get("id").and_then(Value::as_str),
                info.get("role").and_then(Value::as_str),
            ) else {
                return BusOutcome::Continue;
            };
            if session == session_id {
                main_feed
                    .assistant_messages
                    .entry(message.to_owned())
                    .or_insert(role == "assistant");
                // Token usage rides the assistant message; the last one
                // before idle wins, emitted right before Done.
                if role == "assistant"
                    && let Some(tokens) = info.get("tokens")
                {
                    let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
                    let output = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
                    if input > 0 || output > 0 {
                        *pending_usage = Some(AgentEvent::Usage {
                            input_tokens: input,
                            output_tokens: output,
                        });
                    }
                }
                let events = replay_pending(main_feed, message, true, turn);
                return forward(event_tx, events).await;
            }
            if let Some(child) = children.get_mut(session) {
                child
                    .feed
                    .assistant_messages
                    .entry(message.to_owned())
                    .or_insert(role == "assistant");
                // A NEW user message on a settled child is a steer resuming
                // it: un-latch so resumed traffic streams to the same chip.
                if role == "user" && child.done {
                    child.done = false;
                }
                let events = replay_pending(&mut child.feed, message, false, turn);
                let parent = child.parent_tool_use_id.clone();
                let tagged = events.into_iter().map(|ev| tag(&parent, ev)).collect();
                return forward(event_tx, tagged).await;
            }
            BusOutcome::Continue
        }
        "message.part.updated" => {
            let part = props.get("part").unwrap_or(&Value::Null);
            let Some(session) = part.get("sessionID").and_then(Value::as_str) else {
                return BusOutcome::Continue;
            };
            if session == session_id {
                let events = part_snapshot_events(
                    main_feed,
                    part,
                    true,
                    Some((children, pending_spawns, unbound_children)),
                );
                mark_content(turn, &events);
                let mut settle: Vec<AgentEvent> = Vec::new();
                // A completed `task` part settles its child chip.
                if let Some((child_session, failed)) = task_completion(part) {
                    let by_meta = children
                        .get_mut(&child_session)
                        .map(|c| (child_session.clone(), c));
                    let target = match by_meta {
                        Some(v) => Some(v),
                        None => {
                            // No metadata binding: settle whichever child
                            // streams to this part's chip.
                            let part_id = part.get("id").and_then(Value::as_str).unwrap_or("");
                            children
                                .iter_mut()
                                .find(|(_, c)| c.parent_tool_use_id == part_id)
                                .map(|(id, c)| (id.clone(), c))
                        }
                    };
                    if let Some((_, child)) = target
                        && !child.done
                    {
                        child.done = true;
                        settle.push(tag(
                            &child.parent_tool_use_id,
                            AgentEvent::Done {
                                status: if failed {
                                    DoneStatus::Errored
                                } else {
                                    DoneStatus::Completed
                                },
                                result: None,
                                error: None,
                                session_id: None,
                            },
                        ));
                    }
                }
                let mut all = events;
                all.extend(settle);
                return forward(event_tx, all).await;
            }
            if let Some(child) = children.get_mut(session) {
                if child.done {
                    return BusOutcome::Continue;
                }
                let events = part_snapshot_events(&mut child.feed, part, false, None);
                let parent = child.parent_tool_use_id.clone();
                let tagged = events.into_iter().map(|ev| tag(&parent, ev)).collect();
                return forward(event_tx, tagged).await;
            }
            BusOutcome::Continue
        }
        "message.part.delta" => {
            let (Some(session), Some(part_id), Some(delta)) = (
                props.get("sessionID").and_then(Value::as_str),
                props.get("partID").and_then(Value::as_str),
                props.get("delta").and_then(Value::as_str),
            ) else {
                return BusOutcome::Continue;
            };
            if props.get("field").and_then(Value::as_str) != Some("text") {
                return BusOutcome::Continue;
            }
            if session == session_id {
                let events = part_delta_events(main_feed, props, part_id, delta);
                mark_content(turn, &events);
                return forward(event_tx, events).await;
            }
            if let Some(child) = children.get_mut(session) {
                if child.done {
                    return BusOutcome::Continue;
                }
                let events = part_delta_events(&mut child.feed, props, part_id, delta);
                let parent = child.parent_tool_use_id.clone();
                let tagged = events.into_iter().map(|ev| tag(&parent, ev)).collect();
                return forward(event_tx, tagged).await;
            }
            BusOutcome::Continue
        }
        "permission.asked" => {
            // Parity with every other driver: sessions run unattended, so
            // permissions auto-approve ("always" also whitelists the
            // pattern, cutting future asks). Child sessions included — the
            // ACP layer silently dropped those and subagents hung.
            let Some(id) = props.get("id").and_then(Value::as_str) else {
                return BusOutcome::Continue;
            };
            let session = event_session.unwrap_or(session_id).to_owned();
            let reply_path = format!("/permission/{id}/reply");
            let fallback_path = format!("/session/{session}/permissions/{id}");
            let base = server.base.clone();
            let auth = server.auth.clone();
            let dir_owned = dir.map(str::to_owned);
            tokio::spawn(async move {
                let server = Server {
                    child: None,
                    base,
                    auth,
                    client: http_client(),
                    stderr_tail: crate::StderrTail::default(),
                };
                if server
                    .post_json(
                        &reply_path,
                        dir_owned.as_deref(),
                        &json!({ "reply": "always" }),
                    )
                    .await
                    .is_err()
                {
                    let _ = server
                        .post_json(
                            &fallback_path,
                            dir_owned.as_deref(),
                            &json!({ "response": "always" }),
                        )
                        .await;
                }
            });
            BusOutcome::Continue
        }
        "question.asked" => {
            let Some(id) = props.get("id").and_then(Value::as_str) else {
                return BusOutcome::Continue;
            };
            let questions = map_questions(props);
            if questions.is_empty() {
                return BusOutcome::Continue;
            }
            if !send(
                event_tx,
                AgentEvent::InputRequested {
                    request_id: id.to_owned(),
                    questions: questions.clone(),
                },
            )
            .await
            {
                return BusOutcome::ConsumerGone;
            }
            let rx = (request_input)(questions.clone());
            let base = server.base.clone();
            let auth = server.auth.clone();
            let dir_owned = dir.map(str::to_owned);
            let request_id = id.to_owned();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let server = Server {
                    child: None,
                    base,
                    auth,
                    client: http_client(),
                    stderr_tail: crate::StderrTail::default(),
                };
                let reply = match rx.await {
                    Ok(answers) => {
                        let ordered: Vec<Vec<String>> = questions
                            .iter()
                            .map(|q| {
                                answers
                                    .iter()
                                    .find(|a| a.question_id == q.id)
                                    .map(|a| a.labels.clone())
                                    .unwrap_or_default()
                            })
                            .collect();
                        server
                            .post_json(
                                &format!("/question/{request_id}/reply"),
                                dir_owned.as_deref(),
                                &json!({ "answers": ordered }),
                            )
                            .await
                    }
                    Err(_) => {
                        server
                            .post_json(
                                &format!("/question/{request_id}/reject"),
                                dir_owned.as_deref(),
                                &Value::Null,
                            )
                            .await
                    }
                };
                if let Err(e) = reply {
                    tracing::debug!(
                        target: "zeron_harness::opencode",
                        "question reply failed: {e}"
                    );
                }
                let _ = tx
                    .send(Ok(AgentEvent::InputResolved {
                        request_id: request_id.clone(),
                    }))
                    .await;
            });
            BusOutcome::Continue
        }
        _ => BusOutcome::Continue,
    }
}

async fn forward(
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    events: Vec<AgentEvent>,
) -> BusOutcome {
    for ev in events {
        if event_tx.send(Ok(ev)).await.is_err() {
            return BusOutcome::ConsumerGone;
        }
    }
    BusOutcome::Continue
}

fn mark_content(turn: &mut TurnState, events: &[AgentEvent]) {
    if turn.active
        && events.iter().any(|ev| {
            matches!(
                ev,
                AgentEvent::TextDelta { .. }
                    | AgentEvent::ReasoningDelta { .. }
                    | AgentEvent::ToolCall { .. }
            )
        })
    {
        turn.saw_content = true;
    }
}

/// A completed/errored `task` part → (child session id, failed).
fn task_completion(part: &Value) -> Option<(String, bool)> {
    if part.get("type").and_then(Value::as_str) != Some("tool")
        || part.get("tool").and_then(Value::as_str) != Some("task")
    {
        return None;
    }
    let state = part.get("state")?;
    let status = state.get("status").and_then(Value::as_str)?;
    if !matches!(status, "completed" | "error") {
        return None;
    }
    let child = state
        .get("metadata")
        .and_then(|m| m.get("sessionId").or_else(|| m.get("sessionID")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some((child.to_owned(), status == "error"))
}

/// Bind a fresh child session to a spawn chip: description match against the
/// child title (`"{description} (@{agent} subagent)"`), else FIFO.
fn bind_child(
    children: &mut HashMap<String, ChildRun>,
    pending: &mut VecDeque<PendingSpawn>,
    child_id: &str,
    title: &str,
) -> bool {
    let ix = pending
        .iter()
        .position(|p| !p.description.is_empty() && title.starts_with(&p.description))
        .or(if pending.is_empty() { None } else { Some(0) });
    match ix.and_then(|i| pending.remove(i)) {
        Some(p) => {
            children.insert(
                child_id.to_owned(),
                ChildRun {
                    parent_tool_use_id: p.tool_part_id,
                    feed: SessionFeed::default(),
                    done: false,
                },
            );
            true
        }
        None => false,
    }
}

/// Replay parts that raced ahead of their message's role fact.
fn replay_pending(
    feed: &mut SessionFeed,
    message: &str,
    is_main: bool,
    turn: &mut TurnState,
) -> Vec<AgentEvent> {
    let held: Vec<Value> = std::mem::take(&mut feed.pending_parts)
        .into_iter()
        .filter(|part| part.get("messageID").and_then(Value::as_str) == Some(message))
        .collect();
    let events: Vec<AgentEvent> = held
        .iter()
        .flat_map(|part| part_snapshot_events(feed, part, is_main, None))
        .collect();
    if is_main {
        mark_content(turn, &events);
    }
    events
}

/// Spawn-registration context for the MAIN feed (`task` parts register
/// pending chips; child feeds pass `None` — no recursive viz).
type SpawnCtx<'a> = (
    &'a mut HashMap<String, ChildRun>,
    &'a mut VecDeque<PendingSpawn>,
    &'a mut HashMap<String, String>,
);

/// A part snapshot: emit whatever text extends what already streamed, open /
/// resolve tool chips. Snapshots and deltas interleave — `emitted` (bytes of
/// part text already sent) is the dedup line between them.
fn part_snapshot_events(
    feed: &mut SessionFeed,
    part: &Value,
    is_main: bool,
    spawn_ctx: Option<SpawnCtx<'_>>,
) -> Vec<AgentEvent> {
    let Some(part_id) = part.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let message_id = part
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" | "reasoning" => {
            if kind == "text" && feed.assistant_messages.get(message_id).is_none() {
                // Role unknown: hold the part instead of guessing (dedup by
                // part id — snapshots re-deliver).
                if !feed
                    .pending_parts
                    .iter()
                    .any(|p| p.get("id").and_then(Value::as_str) == Some(part_id))
                {
                    feed.pending_parts.push(part.clone());
                }
                return Vec::new();
            }
            if kind == "text" && feed.assistant_messages.get(message_id) == Some(&false) {
                // A user-role text part. On the MAIN feed it is our own
                // prompt echo (the engine already wrote the user entry);
                // on a child feed it is the message INTO the child — its
                // spawn prompt or a steer, rendered as a user entry.
                if is_main {
                    return Vec::new();
                }
                let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                if text.trim().is_empty() {
                    return Vec::new();
                }
                let entry = feed
                    .parts
                    .entry(part_id.to_owned())
                    .or_insert_with(|| PartState {
                        kind: kind.to_owned(),
                        ..PartState::default()
                    });
                if entry.emitted > 0 {
                    return Vec::new();
                }
                entry.emitted = text.len();
                return vec![AgentEvent::UserMessage {
                    text: text.to_owned(),
                }];
            }
            if feed.assistant_messages.get(message_id) != Some(&true) {
                // Reasoning ahead of its message.updated: hold it too.
                if kind == "reasoning" {
                    if !feed
                        .pending_parts
                        .iter()
                        .any(|p| p.get("id").and_then(Value::as_str) == Some(part_id))
                    {
                        feed.pending_parts.push(part.clone());
                    }
                }
                return Vec::new();
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            let entry = feed
                .parts
                .entry(part_id.to_owned())
                .or_insert_with(|| PartState {
                    kind: kind.to_owned(),
                    ..PartState::default()
                });
            let Some(suffix) = text
                .get(entry.emitted..)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                // Shorter (or mid-char) snapshot: a rewrite this decoder
                // doesn't model — drop it rather than duplicate text.
                return Vec::new();
            };
            entry.emitted = text.len();
            vec![if entry.kind == "reasoning" {
                AgentEvent::ReasoningDelta { text: suffix }
            } else {
                AgentEvent::TextDelta { text: suffix }
            }]
        }
        "tool" => {
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or_default();
            let status = part
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = part
                .get("state")
                .and_then(|s| s.get("input"))
                .cloned()
                .unwrap_or(Value::Null);
            // The task chip's stable id is the PART id (the completion and
            // the child settle key on it); ordinary tools key on callID.
            let call_id = if tool == "task" && is_main {
                part_id.to_owned()
            } else {
                part.get("callID")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(part_id)
                    .to_owned()
            };
            let entry = feed
                .parts
                .entry(part_id.to_owned())
                .or_insert_with(|| PartState {
                    kind: "tool".to_owned(),
                    ..PartState::default()
                });
            let mut events = Vec::new();
            let has_input = input.as_object().is_some_and(|o| !o.is_empty());
            if !entry.tool_started && (has_input || matches!(status, "completed" | "error")) {
                entry.tool_started = true;
                events.push(AgentEvent::ToolCall {
                    id: call_id.clone(),
                    call: oc_tool_call(tool, &input),
                });
                // A task spawn on the MAIN feed registers a pending chip so
                // the child's session.created (or its metadata) can bind.
                if tool == "task"
                    && is_main
                    && let Some((children, pending, unbound)) = spawn_ctx
                {
                    register_spawn(children, pending, unbound, part, part_id, &input);
                }
            }
            if entry.tool_started && !entry.tool_done && matches!(status, "completed" | "error") {
                entry.tool_done = true;
                let output = part
                    .get("state")
                    .and_then(|s| {
                        s.get("output")
                            .or_else(|| s.get("error"))
                            .and_then(Value::as_str)
                    })
                    .filter(|t| !t.is_empty())
                    .map(|t| cap_text(t, OUTPUT_CAP));
                events.push(AgentEvent::ToolResult {
                    id: call_id,
                    is_error: status == "error",
                    output,
                    diff: None,
                });
            }
            events
        }
        // step-start / step-finish / snapshot / patch bookkeeping: not
        // transcript.
        _ => Vec::new(),
    }
}

/// Register a `task` spawn chip and bind it eagerly when the tool's own
/// metadata already names the child session (opencode stamps
/// `state.metadata.sessionId` at spawn).
fn register_spawn(
    children: &mut HashMap<String, ChildRun>,
    pending: &mut VecDeque<PendingSpawn>,
    unbound: &mut HashMap<String, String>,
    part: &Value,
    part_id: &str,
    input: &Value,
) {
    let known = pending.iter().any(|p| p.tool_part_id == part_id)
        || children.values().any(|c| c.parent_tool_use_id == part_id);
    if known {
        return;
    }
    let child_id = part
        .get("state")
        .and_then(|s| s.get("metadata"))
        .and_then(|m| m.get("sessionId").or_else(|| m.get("sessionID")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !child_id.is_empty() && !children.contains_key(child_id) {
        unbound.remove(child_id);
        children.insert(
            child_id.to_owned(),
            ChildRun {
                parent_tool_use_id: part_id.to_owned(),
                feed: SessionFeed::default(),
                done: false,
            },
        );
        return;
    }
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // A child that raced ahead of its chip binds now.
    let matched = unbound
        .iter()
        .find(|(_, title)| !description.is_empty() && title.starts_with(&description))
        .map(|(id, _)| id.clone())
        .or_else(|| {
            (unbound.len() == 1)
                .then(|| unbound.keys().next().cloned())
                .flatten()
        });
    if let Some(id) = matched {
        unbound.remove(&id);
        children.insert(
            id,
            ChildRun {
                parent_tool_use_id: part_id.to_owned(),
                feed: SessionFeed::default(),
                done: false,
            },
        );
        return;
    }
    pending.push_back(PendingSpawn {
        tool_part_id: part_id.to_owned(),
        description,
    });
}

/// A text delta appends to its part. Deltas follow the part's opening
/// `message.part.updated` (which fixes the kind); an unknown part defaults
/// to assistant text only when its message is known assistant.
fn part_delta_events(
    feed: &mut SessionFeed,
    props: &Value,
    part_id: &str,
    delta: &str,
) -> Vec<AgentEvent> {
    if delta.is_empty() {
        return Vec::new();
    }
    let message_id = props
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if feed.assistant_messages.get(message_id) != Some(&true) {
        return Vec::new();
    }
    let entry = feed
        .parts
        .entry(part_id.to_owned())
        .or_insert_with(|| PartState {
            kind: "text".to_owned(),
            ..PartState::default()
        });
    if entry.kind == "tool" {
        return Vec::new();
    }
    entry.emitted += delta.len();
    vec![if entry.kind == "reasoning" {
        AgentEvent::ReasoningDelta {
            text: delta.to_owned(),
        }
    } else {
        AgentEvent::TextDelta {
            text: delta.to_owned(),
        }
    }]
}

/// `question.asked` → the input panel's questions (ids are positional).
fn map_questions(props: &Value) -> Vec<UserInputQuestion> {
    props
        .get("questions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .filter_map(|(ix, q)| {
                    let question = q.get("question").and_then(Value::as_str)?;
                    Some(UserInputQuestion {
                        id: format!("q{ix}"),
                        header: q
                            .get("header")
                            .and_then(Value::as_str)
                            .unwrap_or("Question")
                            .to_owned(),
                        question: question.to_owned(),
                        options: q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|opts| {
                                opts.iter()
                                    .filter_map(|o| o.get("label").and_then(Value::as_str))
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multi_select: q.get("multiple").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cap for tool outputs entering the event stream (journal keeps the rest).
const OUTPUT_CAP: usize = 4096;

fn cap_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Type an opencode-native tool invocation.
fn oc_tool_call(name: &str, input: &Value) -> ToolCall {
    let s = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    match name {
        "bash" => ToolCall::Exec {
            command: s(&["command"]).unwrap_or_default(),
        },
        "read" => ToolCall::ReadFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
        },
        "write" => ToolCall::WriteFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
            content: s(&["content"]),
        },
        "edit" => ToolCall::EditFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
            old_string: s(&["oldString", "old_string"]),
            new_string: s(&["newString", "new_string"]),
        },
        "patch" => ToolCall::ApplyPatch {
            path: s(&["filePath", "file_path", "path"]),
        },
        "grep" => ToolCall::Search {
            pattern: s(&["pattern"]).unwrap_or_default(),
            path: s(&["path", "include"]),
        },
        "glob" => ToolCall::Glob {
            pattern: s(&["pattern"]).unwrap_or_default(),
        },
        "webfetch" => ToolCall::WebFetch {
            url: s(&["url"]).unwrap_or_default(),
            prompt: None,
        },
        "websearch" => ToolCall::WebSearch {
            query: s(&["query"]).unwrap_or_default(),
        },
        "todowrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: t
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // The genus-gated spawn naming every driver shares.
        "task" => ToolCall::Unknown {
            name: s(&["description"])
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: (!input.is_null()).then(|| input.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: (!input.is_null()).then(|| input.clone()),
        },
    }
}

// ---------------------------------------------------------------------------
// SSE bus reader
// ---------------------------------------------------------------------------

/// Tail `/global/event` into the session loop. Reconnects on transient
/// drops (the server is our own child on loopback); past the budget the
/// loop learns via [`BusMsg::Disconnected`] and errors the run — missed
/// frames mean the transcript can no longer be trusted.
async fn bus_task(base: String, auth: Option<String>, tx: mpsc::Sender<BusMsg>) {
    let client = http_client();
    let url = format!("{base}/global/event");
    let mut failures: u32 = 0;
    loop {
        if tx.is_closed() {
            return;
        }
        let mut req = client.get(&url);
        if let Some(auth) = &auth {
            req = req.header(reqwest::header::AUTHORIZATION, auth.clone());
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                failures = 0;
                stream_bus(&tx, resp).await;
                if tx.is_closed() {
                    return;
                }
            }
            _ => {}
        }
        // (stream_bus sends Connected itself once the first frame lands —
        // an accepted-but-parked boot-window connection must not count.)
        failures += 1;
        if failures > BUS_RECONNECT_ATTEMPTS {
            let _ = tx.send(BusMsg::Disconnected).await;
            return;
        }
        tokio::time::sleep(BUS_RECONNECT_DELAY).await;
    }
}

async fn stream_bus(tx: &mpsc::Sender<BusMsg>, resp: reqwest::Response) {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut announced = false;
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            return;
        };
        if !announced {
            announced = true;
            if tx.send(BusMsg::Connected).await.is_err() {
                return;
            }
        }
        buf.extend_from_slice(&bytes);
        // SSE frames are blank-line separated; each data line is one event.
        while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<u8> = buf.drain(..pos + 2).collect();
            let Ok(frame) = std::str::from_utf8(&frame) else {
                continue;
            };
            for line in frame.lines() {
                let Some(data) = line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if tx.send(BusMsg::Event(event)).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
