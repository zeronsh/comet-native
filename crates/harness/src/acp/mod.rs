//! ACP harness: spawns an Agent Client Protocol agent (JSON-RPC 2.0 over
//! stdio, protocol v1) and maps its session updates onto [`AgentEvent`]s.
//!
//! KEPT ONLY for agents built ground-up on ACP: Grok ([`AcpHarness::grok`],
//! `grok agent stdio`), Devin ([`AcpHarness::devin`], `devin acp`) and Hermes
//! ([`AcpHarness::hermes`], `hermes acp`) — plus pi
//! ([`AcpHarness::pi`]) via the community `pi-acp` adapter until a native
//! driver exists. Claude, Codex and Cursor moved to native drivers
//! ([`crate::ClaudeHarness`], [`crate::CodexHarness`], [`crate::CursorHarness`])
//! after adapter-mediated ACP kept manufacturing done-status bugs the native
//! wires don't have (turn-hold bookkeeping vs the CLI's own eager result).
//!
//! - `initialize` (protocolVersion 1, fs/terminal capabilities declined) →
//!   `session/new`, or `session/load` with a fresh-session fallback when
//!   resuming; replayed history during a load is dropped (the doc already
//!   holds it).
//! - `session/prompt` owns the turn: its response's `stopReason` ends the
//!   turn (`cancelled` → Interrupted, `refusal` → Errored, else Completed).
//! - `session/update` notifications normalize per [`normalize::map_update`].
//! - Permission requests auto-accept with the agent's preferred allow option
//!   (zeron sessions run unattended); question-shaped requests block on the
//!   engine's input bridge.
//! - Steering: agents advertising `_session/steering` get mid-turn injection;
//!   others queue steers and deliver them as the next `session/prompt` at the
//!   turn boundary. The session stays parked between turns while the
//!   steering mailbox lives.
//! - Interrupt: `session/cancel`, escalating SIGTERM → SIGKILL; the stream
//!   always ends with `Done { status: Interrupted }`.

mod devin_models;
mod normalize;
mod subagent;
mod subagent_devin;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ModelOption, ModelOptionChoice, ReasoningLevel,
    RunRequest, SlashCommand, SteeringMode, UserInputAnswer, UserInputQuestion,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};
use normalize::{map_update, parse_commands, preferred_allow_option};
use subagent::SubagentTracker;
use subagent_devin::DevinTracker;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-agent configuration: which binary to spawn and what to tell the picker.
struct AcpAgentSpec {
    id: HarnessId,
    display_name: &'static str,
    /// Binary name searched on PATH (and platform install dirs).
    executable: &'static str,
    /// Env var overriding executable resolution (tests, custom installs).
    env_override: &'static str,
    /// Arguments that put the binary in ACP-serving mode.
    args: &'static [&'static str],
    /// Pinned npm package (`name@version`) installed ONCE into the managed
    /// adapters dir when the binary isn't already present — the launch then
    /// spawns `node <entry>` directly, keeping npm (and every way a user's
    /// npm state can break) out of chat turns. See [`crate::adapter_install`].
    npm_package: Option<&'static str>,
    /// Extra install locations to probe after PATH.
    extra_paths: fn() -> Vec<PathBuf>,
    /// The agent's own CLI binary (`claude`, `codex`, …) — what "installed"
    /// means to the user. Distinct from `executable` where the spawned adapter
    /// wraps the CLI (`claude-agent-acp`, `codex-acp`, `pi-acp`), and the npx
    /// fallback deliberately doesn't count: npx can fetch an adapter on
    /// demand, but an absent CLI still means no logins/config to drive.
    cli_executable: &'static str,
    /// Extra install locations probed for [`Self::cli_executable`].
    cli_extra_paths: fn() -> Vec<PathBuf>,
    /// Search summary + install hint for the NotInstalled error.
    install_hint: &'static str,
    models: fn() -> Vec<Model>,
    steering_mode: SteeringMode,
    /// Effort ladder surfaced in the picker; applied per session via the
    /// `thought_level` config option (must mirror the registry descriptor).
    reasoning_levels: &'static [ReasoningLevel],
    /// Transform applied to the initial prompt and every steer — Claude's
    /// Ultrathink is a prompt-prefix convention, not an effort flag.
    prompt_transform: fn(Option<ReasoningLevel>, &str) -> String,
    /// Preference-ordered `thought_level` value ids for the run's reasoning
    /// (per-agent clamping, e.g. Claude xhigh→max off the xhigh family). The
    /// first value the agent actually advertises wins.
    effort_values: fn(Option<ReasoningLevel>, Option<&str>) -> Vec<&'static str>,
    /// Levels appended to a DISCOVERED model's non-empty ladder: modes the
    /// wire can't advertise because they aren't `thought_level` values.
    ladder_extras: &'static [ReasoningLevel],
    /// The agent emits `_x.ai/session/prompt_complete` (Grok): treat it as
    /// the AUTHORITATIVE turn end — grok's `session/prompt` RPC can hang
    /// silently after the turn really finished (field reports: total silent
    /// non-response on some machines). The prompt response stays as the
    /// fallback; whichever lands first settles the turn, exactly once.
    prompt_complete_extension: bool,
    /// Bound on prompt-send → FIRST sign of life on the wire. `None`
    /// disables. Grok's healthy runs acknowledge a prompt within
    /// milliseconds (queue bookkeeping precedes any model work), so total
    /// silence past this window is a wedged agent (a stale shared leader,
    /// a dead update check) — surface a visible error chip instead of
    /// indefinite Working.
    prompt_stall: Option<Duration>,
    /// Agent-specific advice appended to the stall error chip: what a wedge
    /// usually means for THIS agent and what the user can check.
    stall_hint: &'static str,
}

fn identity_transform(_reasoning: Option<ReasoningLevel>, text: &str) -> String {
    text.to_owned()
}

/// PATH + login-shell + extra dirs + node-version-manager scan for a binary.
pub(crate) fn find_on_paths(exe: &str, extra: Vec<PathBuf>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(shell_path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe)),
        );
    }
    candidates.extend(extra);
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// Generic effort ladder for agents without their own clamping rules.
fn default_effort_values(
    reasoning: Option<ReasoningLevel>,
    _model: Option<&str>,
) -> Vec<&'static str> {
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

/// npm-global bin dirs for an adapter binary (`npm i -g` installs).
fn npm_global_paths(exe: &'static str) -> fn() -> Vec<PathBuf> {
    // fn pointers can't capture; probe the fixed npm-global locations and
    // append the exe at call time via a small per-exe shim table.
    match exe {
        "pi-acp" => || npm_global_bins("pi-acp"),
        _ => || Vec::new(),
    }
}

fn npm_global_bins(exe: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin").join(exe));
        dirs.push(home.join(".npm-global").join("bin").join(exe));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    dirs.push(PathBuf::from("/usr/local/bin").join(exe));
    dirs
}

fn grok_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin").join("grok"));
        dirs.push(home.join(".grok").join("bin").join("grok"));
        dirs.push(home.join(".npm-global").join("bin").join("grok"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/grok"));
    dirs.push(PathBuf::from("/usr/local/bin/grok"));
    dirs
}

fn grok_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Grok,
        display_name: "Grok",
        executable: "grok",
        env_override: "GROK_EXECUTABLE",
        // Flag placement verified against grok 1.0.4: `--no-auto-update` is
        // TOP-LEVEL (before the subcommand) and kills the launch-time update
        // check (a silent multi-second network stall); `--no-leader` lives on
        // the `agent` subcommand and starts a fresh agent even when
        // `[cli] use_leader` is set — leader mode ATTACHES `agent stdio` to a
        // shared process via ~/.grok/leader.sock, so a wedged/stale leader
        // (the user's TUI) reads as total silent non-response in zeron.
        args: &["--no-auto-update", "agent", "--no-leader", "stdio"],
        npm_package: Some("@xai-official/grok@1.0.4"),
        extra_paths: grok_install_paths,
        cli_executable: "grok",
        cli_extra_paths: grok_install_paths,
        install_hint: "grok (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.grok/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, and \
             fnm/nvm/volta/pnpm/bun install dirs; install with \
             `curl -fsSL https://x.ai/cli/install.sh | bash` or \
             `npm install -g @xai-official/grok`; set GROK_EXECUTABLE to override)",
        models: || {
            vec![Model {
                id: "grok-4.5".into(),
                label: "Grok 4.5".into(),
                description: Some("xAI's coding model — 500k context".into()),
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ],
                options: Vec::new(),
            }]
        },
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        // Grok Build's advertised efforts (default high); applied through the
        // session's `thought_level` config option.
        reasoning_levels: &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        // Verified live (1.0.4): the notification fires with the echoed
        // `_meta.promptId`, just ahead of the RPC response.
        prompt_complete_extension: true,
        prompt_stall: Some(Duration::from_secs(30)),
        stall_hint: "The agent process is likely wedged — a stale shared leader \
             process or a hung startup check; zeron launches it with --no-leader \
             and --no-auto-update to avoid both.",
    }
}

fn devin_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // The official installer's launcher symlink (the binary lives below
        // ~/.local/share/devin/cli/_versions).
        dirs.push(home.join(".local").join("bin").join("devin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/devin"));
    dirs.push(PathBuf::from("/usr/local/bin/devin"));
    dirs
}

fn devin_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Devin,
        display_name: "Devin",
        executable: "devin",
        env_override: "DEVIN_EXECUTABLE",
        args: &["acp"],
        // Native ACP server — no adapter package in between.
        npm_package: None,
        extra_paths: devin_install_paths,
        cli_executable: "devin",
        cli_extra_paths: devin_install_paths,
        install_hint: "devin (searched PATH, the login shell's PATH, ~/.local/bin, \
             /opt/homebrew/bin, and /usr/local/bin; install with \
             `curl -fsSL https://cli.devin.ai/install.sh | bash` or \
             `brew install --cask devin-cli`, then `devin auth login`; set \
             DEVIN_EXECUTABLE to override)",
        // Legacy metadata only: discovery uses `devin models list` because
        // session/new starts with a stale catalog. Effort is baked into ids.
        // These ids were live-verified with CLI 3000.6.14; `swe-1-7-medium`
        // is the session default the server reports.
        models: || {
            vec![
                Model {
                    id: "swe-1-7-medium".into(),
                    label: "SWE-1.7 Medium".into(),
                    description: Some("Devin's default coding model".into()),
                    reasoning_levels: Vec::new(),
                    options: Vec::new(),
                },
                Model {
                    id: "claude-fable-5-1-high".into(),
                    label: "Claude Fable 5.1 High".into(),
                    description: Some("Anthropic's frontier model through Devin".into()),
                    reasoning_levels: Vec::new(),
                    options: Vec::new(),
                },
                Model {
                    id: "adaptive".into(),
                    label: "Adaptive".into(),
                    description: Some("Devin picks the model per request".into()),
                    reasoning_levels: Vec::new(),
                    options: Vec::new(),
                },
            ]
        },
        steering_mode: SteeringMode::TurnBoundary,
        // Effort is encoded in Devin's advertised model ids, not a separate
        // thought_level config option.
        reasoning_levels: &[],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
    }
}

fn hermes_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin").join("hermes"));
        dirs.push(home.join(".hermes").join("bin").join("hermes"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/hermes"));
    dirs.push(PathBuf::from("/usr/local/bin/hermes"));
    dirs
}

fn hermes_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Hermes,
        display_name: "Hermes",
        executable: "hermes",
        env_override: "HERMES_EXECUTABLE",
        args: &["acp"],
        // Python/uv install — no npm fallback exists.
        npm_package: None,
        extra_paths: hermes_install_paths,
        cli_executable: "hermes",
        cli_extra_paths: hermes_install_paths,
        install_hint: "hermes (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.hermes/bin, /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun \
             install dirs; install with \
             `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash`, then \
             `cd ~/.hermes/hermes-agent && uv pip install -e '.[acp]'` for the ACP \
             server; set HERMES_EXECUTABLE to override)",
        // Hermes derives its model list from the providers the user has
        // authenticated (`hermes model`); these are the Nous flagships every
        // portal account gets. Ids the agent doesn't advertise are skipped by
        // the config-option set, falling back to the agent's own default.
        models: || {
            vec![
                Model {
                    id: "hermes-4-405b".into(),
                    label: "Hermes 4 405B".into(),
                    description: Some("Nous Research's hybrid-reasoning flagship".into()),
                    reasoning_levels: Vec::new(),
                    options: Vec::new(),
                },
                Model {
                    id: "hermes-4-70b".into(),
                    label: "Hermes 4 70B".into(),
                    description: Some("Faster Hermes 4 — same post-training, 70B".into()),
                    reasoning_levels: Vec::new(),
                    options: Vec::new(),
                },
            ]
        },
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        // Hermes exposes no effort config over ACP today (hybrid reasoning is
        // model-internal); revisit when the adapter advertises a ladder.
        reasoning_levels: &[],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
    }
}

fn pi_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Pi,
        display_name: "Pi",
        executable: "pi-acp",
        env_override: "PI_ACP_EXECUTABLE",
        args: &[],
        npm_package: Some("pi-acp@0.0.33"),
        extra_paths: npm_global_paths("pi-acp"),
        cli_executable: "pi",
        cli_extra_paths: || npm_global_bins("pi"),
        install_hint: "pi-acp (searched PATH, the login shell's PATH, npm global bins, \
             and fnm/nvm/volta/pnpm/bun install dirs; zeron installs the pinned \
             pi-acp automatically when npm is available — the pi CLI itself is \
             still required, `npm install -g --ignore-scripts \
             @earendil-works/pi-coding-agent`; set PI_ACP_EXECUTABLE to override)",
        // pi routes models through its own provider config (~/.pi); the picker
        // advertises the pass-through entry and pi keeps whatever the user set
        // up. Unknown ids are skipped by the config-option set.
        models: || {
            vec![Model {
                id: "default".into(),
                label: "pi default".into(),
                description: Some("Runs the model configured in pi (`pi` settings)".into()),
                reasoning_levels: vec![
                    ReasoningLevel::Minimal,
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                    ReasoningLevel::Max,
                ],
                options: Vec::new(),
            }]
        },
        // The adapter has no `_session/steering` extension: turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        // pi's thinking ladder (minimal→max; its extra "off" tier has no zeron
        // equivalent and is left to the agent default).
        reasoning_levels: &[
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
    }
}

/// Background-install managed npm adapters for agents whose CLI is present
/// on this device, so a first chat never pays (or trips over) an npm run.
/// Skips agents whose adapter is already resolvable; failures are logged and
/// retried on the next daemon start or blocking launch. A no-op outside a
/// tokio runtime.
pub fn prewarm_managed_adapters() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    for spec in [grok_spec(), pi_spec()] {
        let Some(pkg) = spec.npm_package else {
            continue;
        };
        let pin = crate::adapter_install::NpmPin::parse(pkg);
        if find_on_paths(spec.executable, (spec.extra_paths)()).is_some()
            || crate::adapter_install::installed_entry(&pin, spec.executable).is_some()
            || find_on_paths(spec.cli_executable, (spec.cli_extra_paths)()).is_none()
            || crate::adapter_install::find_npm().is_none()
        {
            continue;
        }
        let (bin_name, display_name) = (spec.executable, spec.display_name);
        handle.spawn(async move {
            match crate::adapter_install::ensure_installed(pin, bin_name, display_name).await {
                Ok(entry) => tracing::info!(
                    target: "zeron_harness::adapter_install",
                    adapter = %entry.display(),
                    "prewarmed {display_name} ACP adapter"
                ),
                Err(e) => tracing::warn!(
                    target: "zeron_harness::adapter_install",
                    "prewarm of the {display_name} ACP adapter failed: {e}"
                ),
            }
        });
    }
}

/// A resolved launch: a concrete program, or a managed npm adapter that may
/// still need installing (see [`AcpHarness::resolve_program`]).
enum Launch {
    Program(PathBuf, Vec<String>),
    Managed {
        pin: crate::adapter_install::NpmPin,
        bin_name: &'static str,
        args: Vec<String>,
    },
}

/// The ACP harness. Construct with [`AcpHarness::grok`]; tests point it at a
/// fake agent with [`AcpHarness::with_executable`].
pub struct AcpHarness {
    spec: AcpAgentSpec,
    executable: Option<PathBuf>,
    /// Override of the agent's on-disk sessions root (grok's
    /// `~/.grok/sessions`), where subagent transcripts are tailed from.
    sessions_root: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Bound on the initialize → session handshake; a hang past it errors the
    /// run instead of spinning "Working" forever.
    handshake_timeout: Duration,
    /// Bound on the initialize → session/new probe used to populate the model
    /// picker. OpenCode shares this with its real startup budget because both
    /// paths wait for the same plugin-heavy boot.
    model_discovery_timeout: Duration,
    /// Discovery result cache: the advertised commands survive across calls.
    commands: tokio::sync::OnceCell<Vec<SlashCommand>>,
    /// Model discovery cache: only a successful, non-empty probe is cached,
    /// so a mis-authed agent retries on the next picker open.
    models_cache: tokio::sync::OnceCell<Vec<Model>>,
    /// Coalesce concurrent picker/title probes. Starting several OpenCode
    /// processes at once makes cold plugin loading slower and wastes memory.
    models_probe: tokio::sync::Mutex<()>,
    devin_models: devin_models::Catalog,
}

impl AcpHarness {
    fn with_spec(spec: AcpAgentSpec) -> Self {
        Self {
            spec,
            executable: None,
            sessions_root: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            // Generous: the handshake is local work for every agent
            // (session/load replays from disk), so a hang past this is a
            // wedged agent, not a slow one.
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            model_discovery_timeout: DEFAULT_MODEL_DISCOVERY_TIMEOUT,
            commands: tokio::sync::OnceCell::new(),
            models_cache: tokio::sync::OnceCell::new(),
            models_probe: tokio::sync::Mutex::new(()),
            devin_models: devin_models::Catalog::default(),
        }
    }

    /// Devin (`devin acp`) — Cognition's native ACP server.
    pub fn devin() -> Self {
        Self::with_spec(devin_spec())
    }

    /// Grok Build (`grok agent stdio`) — xAI's native ACP agent.
    pub fn grok() -> Self {
        Self::with_spec(grok_spec())
    }

    /// Hermes Agent (`hermes acp`) — Nous Research's native ACP server.
    pub fn hermes() -> Self {
        Self::with_spec(hermes_spec())
    }

    /// The pi coding agent over ACP — the community `pi-acp` adapter wrapping
    /// pi's RPC mode.
    pub fn pi() -> Self {
        Self::with_spec(pi_spec())
    }

    /// Use a fixed agent binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Test seam: tail subagent transcripts from this sessions root instead
    /// of the agent's real one (`~/.grok/sessions`).
    #[doc(hidden)]
    pub fn with_sessions_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.sessions_root = Some(root.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Tune the handshake bound (tests shrink it; default 120s).
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Tune the model-probe bound (tests shrink it; OpenCode defaults to the
    /// same five-minute, environment-overridable budget as its real startup).
    #[doc(hidden)]
    pub fn with_model_discovery_timeout(mut self, timeout: Duration) -> Self {
        self.model_discovery_timeout = timeout;
        self
    }

    /// Test seam: the program `run` would spawn (the adapter binary, or —
    /// for a managed npm adapter — its installed entry, else npm as the
    /// installer that would run first).
    #[doc(hidden)]
    pub fn launch_program(&self) -> Result<PathBuf, HarnessError> {
        match self.resolve_launch()? {
            Launch::Program(program, _) => Ok(program),
            Launch::Managed { pin, bin_name, .. } => {
                match crate::adapter_install::installed_entry(&pin, bin_name) {
                    Some(entry) => Ok(entry),
                    None => crate::adapter_install::find_npm()
                        .ok_or_else(|| HarnessError::NotInstalled(self.spec.install_hint.into())),
                }
            }
        }
    }

    /// Resolve what to spawn: an explicit/installed adapter binary, or the
    /// managed install of the spec's pinned npm package. `NotInstalled` only
    /// when neither the binary nor the machinery to install it (npm) exists.
    fn resolve_launch(&self) -> Result<Launch, HarnessError> {
        let spec_args: Vec<String> = self.spec.args.iter().map(|a| a.to_string()).collect();
        if let Some(p) = &self.executable {
            return Ok(Launch::Program(p.clone(), spec_args));
        }
        if let Some(p) = std::env::var_os(self.spec.env_override)
            && !p.is_empty()
        {
            return Ok(Launch::Program(PathBuf::from(p), spec_args));
        }
        if let Some(found) = find_on_paths(self.spec.executable, (self.spec.extra_paths)()) {
            return Ok(Launch::Program(found, spec_args));
        }
        if let Some(pkg) = self.spec.npm_package {
            let pin = crate::adapter_install::NpmPin::parse(pkg);
            if crate::adapter_install::installed_entry(&pin, self.spec.executable).is_some()
                || crate::adapter_install::find_npm().is_some()
            {
                return Ok(Launch::Managed {
                    pin,
                    bin_name: self.spec.executable,
                    args: spec_args,
                });
            }
        }
        Err(HarnessError::NotInstalled(self.spec.install_hint.into()))
    }

    /// Resolve to a concrete (program, args), running the managed install if
    /// it hasn't completed yet. `block_on_install: false` (discovery paths)
    /// never waits on npm: it kicks the install in the background and errors
    /// out, so a picker open falls back to the static catalog instead of
    /// stalling for however long a 500MB dependency tree takes to land.
    async fn resolve_program(
        &self,
        block_on_install: bool,
    ) -> Result<(PathBuf, Vec<String>), HarnessError> {
        match self.resolve_launch()? {
            Launch::Program(program, args) => Ok((program, args)),
            Launch::Managed {
                pin,
                bin_name,
                args,
            } => {
                let entry = match crate::adapter_install::installed_entry(&pin, bin_name) {
                    Some(entry) => entry,
                    None if block_on_install => {
                        crate::adapter_install::ensure_installed(
                            pin,
                            bin_name,
                            self.spec.display_name,
                        )
                        .await?
                    }
                    None => {
                        let display_name = self.spec.display_name;
                        tokio::spawn(async move {
                            if let Err(e) = crate::adapter_install::ensure_installed(
                                pin,
                                bin_name,
                                display_name,
                            )
                            .await
                            {
                                tracing::warn!(
                                    target: "zeron_harness::adapter_install",
                                    "background adapter install failed: {e}"
                                );
                            }
                        });
                        return Err(HarnessError::Protocol(format!(
                            "{} adapter is installing in the background",
                            self.spec.display_name
                        )));
                    }
                };
                let (program, mut node_args) = crate::adapter_install::launch_for_entry(&entry)?;
                node_args.extend(args);
                Ok((program, node_args))
            }
        }
    }

    async fn spawn_agent(
        &self,
        cwd: Option<&str>,
        block_on_install: bool,
        extra_args: &[String],
    ) -> Result<(Child, crate::StderrTail), HarnessError> {
        let (exe, args) = self.resolve_program(block_on_install).await?;
        let mut cmd = Command::new(&exe);
        cmd.args(args);
        cmd.args(extra_args);
        crate::compose_child_path(&mut cmd, &exe);
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
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
                    tracing::debug!(target: "zeron_harness::acp", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }
        Ok((child, stderr_tail))
    }

    /// Short-lived discovery run for [`Harness::commands`]: initialize, scan
    /// the response, then try one unauthenticated `session/new` and wait
    /// briefly for `available_commands_update`. Best-effort — an agent that
    /// refuses sessions before login still surfaces whatever the handshake
    /// advertised.
    async fn discover_commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let (mut child, _stderr) = self.spawn_agent(None, false, &[]).await?;
        let (client, mut incoming) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => RpcClient::new(stdin, stdout),
            _ => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol("agent child has no stdio".into()));
            }
        };
        let discovery = async {
            let init = client
                .request("initialize", initialize_params(self.spec.id))
                .await?;
            let mut commands = scan_available_commands(&init);
            if commands.is_empty() {
                let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let session = client
                    .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                    .await;
                if session.is_ok() {
                    // The update usually arrives within milliseconds of the
                    // session response; 2s bounds a quiet agent.
                    let deadline = tokio::time::sleep(Duration::from_secs(2));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            inc = incoming.recv() => match inc {
                                Some(Incoming::Notification { method, params })
                                    if method == "session/update" =>
                                {
                                    let update = params.get("update").cloned().unwrap_or(Value::Null);
                                    if update.get("sessionUpdate").and_then(Value::as_str)
                                        == Some("available_commands_update")
                                    {
                                        commands = parse_commands(update.get("availableCommands"));
                                        break;
                                    }
                                }
                                Some(Incoming::Request { id, .. }) => {
                                    client.respond_error(&id, -32601, "unsupported during discovery");
                                }
                                Some(_) => {}
                                None => break,
                            },
                            _ = &mut deadline => break,
                        }
                    }
                }
            }
            Ok::<Vec<SlashCommand>, HarnessError>(commands)
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("command discovery timed out".into())),
        }
    }

    /// One short-lived probe for the agent's real model list: initialize →
    /// `session/new`, then read the response's first-class `models`
    /// (SessionModelState) with the `model` config option as fallback. The
    /// wire is the source of truth — the spec's static catalog only enriches
    /// matching entries and names the pick when the agent advertises nothing.
    async fn discover_models(&self) -> Result<Vec<Model>, HarnessError> {
        let (mut child, stderr_tail) = self.spawn_agent(None, false, &[]).await?;
        let (client, _incoming) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => RpcClient::new(stdin, stdout),
            _ => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol("agent child has no stdio".into()));
            }
        };
        let discovery = async {
            client
                .request("initialize", initialize_params(self.spec.id))
                .await?;
            let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let session = client
                .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                .await?;
            let mut models = models_from_session(&session, &(self.spec.models)());
            // Prompt-convention modes (Claude Ultrathink) extend any real
            // ladder — never an effort-less model's empty one.
            for model in &mut models {
                if !model.reasoning_levels.is_empty() {
                    for extra in self.spec.ladder_extras {
                        if !model.reasoning_levels.contains(extra) {
                            model.reasoning_levels.push(*extra);
                        }
                    }
                }
            }
            Ok::<Vec<Model>, HarnessError>(models)
        };
        let result = tokio::time::timeout(self.model_discovery_timeout, discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => {
                let mut error = format!(
                    "{} model discovery did not complete within {}s",
                    self.spec.display_name,
                    self.model_discovery_timeout.as_secs()
                );
                if let Some(stderr) = stderr_tail.snapshot() {
                    error.push_str("; stderr: ");
                    error.push_str(&stderr);
                }
                Err(HarnessError::Protocol(error))
            }
        }
    }
}

/// Map an advertised `thought_level` value id onto zeron's ladder.
fn reasoning_from_value(value: &str) -> Option<ReasoningLevel> {
    match norm_id(value).as_str() {
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        "ultra" => Some(ReasoningLevel::Ultra),
        "ultracode" => Some(ReasoningLevel::Ultracode),
        "ultrathink" => Some(ReasoningLevel::Ultrathink),
        _ => None,
    }
}

/// Derive the model list a `session/new` response advertises. The `model`
/// config option's choices come FIRST, the legacy first-class `models` state
/// is only a fallback: the org adapters enumerate one `availableModels` entry
/// per model × effort combination on that deprecated surface (Zed dropped it
/// entirely), while their `configOptions` carry base model ids with effort as
/// a separate `thought_level` option. `[1m]`-suffixed long-context variants
/// collapse into the base model's Context Window trait, matching the static
/// catalogs. Traits come off the wire too — every select/boolean config
/// option outside mode/model/thought_level becomes a `ModelOption` — so
/// unmatched models keep fast mode etc.; the catalog only enriches matched
/// ids with label/description/per-model ladders.
fn models_from_session(session_response: &Value, catalog: &[Model]) -> Vec<Model> {
    let config_options = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default();

    let ladder: Vec<ReasoningLevel> = config_options
        .iter()
        .find(|o| o.get("category").and_then(Value::as_str) == Some("thought_level"))
        .and_then(|o| o.get("options").and_then(Value::as_array))
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("value").and_then(Value::as_str))
                .filter_map(reasoning_from_value)
                .collect()
        })
        .unwrap_or_default();
    let wire_options: Vec<ModelOption> = config_options
        .iter()
        .filter_map(trait_from_config_option)
        .collect();

    let exact = |id: &str| catalog.iter().find(|m| norm_id(&m.id) == norm_id(id));
    // Family-alias catalog row: the claude adapter advertises bare aliases
    // (`opus`, `sonnet`, `haiku`) meaning "the current generation" — match
    // them to the first (flagship-ordered) catalog row of that family so
    // the picker shows the curated label/ladder ("Opus 5") instead of the
    // terse alias. Alphabetic-only ids ONLY: versioned ids
    // (`gpt-5.2-codex`) must never fuzzy-match a foreign row.
    let alias = |id: &str| {
        let norm = norm_id(id);
        (!norm.is_empty() && norm.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| catalog.iter().find(|m| norm_id(&m.id).contains(&norm)))
            .flatten()
    };
    let build = |id: &str,
                 name: Option<&str>,
                 description: Option<&str>,
                 options: Vec<ModelOption>|
     -> Model {
        let exact = exact(id);
        let aliased = if exact.is_none() { alias(id) } else { None };
        let known = exact.or(aliased);
        // The wire name wins for real ids; an ALIAS row's terse wire name
        // ("Opus") loses to the curated family label/description.
        Model {
            id: id.to_owned(),
            label: aliased
                .map(|m| m.label.clone())
                .or_else(|| name.map(str::to_owned))
                .or_else(|| known.map(|m| m.label.clone()))
                .unwrap_or_else(|| id.to_owned()),
            description: aliased
                .and_then(|m| m.description.clone())
                .or_else(|| description.map(str::to_owned))
                .or_else(|| known.and_then(|m| m.description.clone())),
            reasoning_levels: match known.filter(|m| !m.reasoning_levels.is_empty()) {
                Some(m) => m.reasoning_levels.clone(),
                None => ladder.clone(),
            },
            options,
        }
    };

    let model_select: Vec<&Value> = config_options
        .iter()
        .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
        .and_then(|o| o.get("options").and_then(Value::as_array))
        .map(|opts| opts.iter().collect())
        .unwrap_or_default();
    if !model_select.is_empty() {
        let raw_ids: Vec<&str> = model_select
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();
        // `default` is an ALIAS row (Claude Code's "Default (recommended)"),
        // duplicating whichever real model the CLI resolves it to — dropped
        // whenever a real row exists (it read as clutter in the picker, user
        // request). Send-side, a chat that saved `default` still matches the
        // advertised value exactly.
        let has_real = raw_ids.iter().any(|id| norm_id(id) != "default");
        return model_select
            .iter()
            .filter_map(|o| {
                let id = o.get("value").and_then(Value::as_str)?;
                if has_real && norm_id(id) == "default" {
                    return None;
                }
                let name = o.get("name").and_then(Value::as_str);
                let description = o.get("description").and_then(Value::as_str);
                let mut options = wire_options.clone();
                if let Some(base) = strip_context_hint(id) {
                    // A 1M variant with its bare base advertised too folds
                    // into THAT row's Context Window trait (added below).
                    if raw_ids.contains(&base) {
                        return None;
                    }
                    // Orphan 1M variant (`opus[1m]` with no bare `opus` —
                    // the CLI pins the 1M window): present it AS the base
                    // model with the trait defaulting to 1M, instead of a
                    // one-off "Opus (1M context)" row (user request). The
                    // send path recomposes the advertised id via
                    // `pick_model_value`'s compose/family fallback.
                    let mut window = crate::claude::catalog::context_window();
                    window.default_choice = "1m".into();
                    options.push(window);
                    return Some(build(
                        base,
                        name.map(strip_trailing_parenthetical)
                            .filter(|n| !n.is_empty()),
                        description,
                        options,
                    ));
                }
                if raw_ids
                    .iter()
                    .any(|raw| strip_context_hint(raw) == Some(id))
                {
                    options.push(crate::claude::catalog::context_window());
                }
                Some(build(id, name, description, options))
            })
            .collect();
    }

    // Legacy fallback for agents predating session config options. The
    // catalog's own option sets apply here — nothing arrives on the wire.
    session_response
        .get("models")
        .and_then(|m| m.get("availableModels"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            Some(build(
                id,
                m.get("name").and_then(Value::as_str),
                m.get("description").and_then(Value::as_str),
                exact(id).map(|k| k.options.clone()).unwrap_or_default(),
            ))
        })
        .collect()
}

/// A session config option surfaced as a Traits-dropdown section. Mode is
/// zeron's own (forced to the no-prompts choice), model rides the model rows,
/// and thought_level is the Reasoning ladder — everything else the agent
/// advertises (fast mode, collaboration mode, agent persona, …) passes
/// through. `currentValue` doubles as the default: it is the state the
/// session opens in. Booleans render as an off/on select, mirroring the
/// catalogs (zeron never declares the boolean config capability, so adapters
/// send selects, but handle the shape defensively).
fn trait_from_config_option(option: &Value) -> Option<ModelOption> {
    if matches!(
        option.get("category").and_then(Value::as_str),
        Some("mode" | "model" | "thought_level")
    ) {
        return None;
    }
    let id = option.get("id").and_then(Value::as_str)?;
    let label = option.get("name").and_then(Value::as_str).unwrap_or(id);
    match option.get("type").and_then(Value::as_str)? {
        "select" => {
            let choices: Vec<ModelOptionChoice> = option
                .get("options")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|c| {
                    let id = c.get("value").and_then(Value::as_str)?;
                    Some(ModelOptionChoice {
                        id: id.to_owned(),
                        label: c
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_owned(),
                    })
                })
                .collect();
            let default_choice = option
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| choices.first().map(|c| c.id.clone()))?;
            (choices.len() > 1).then(|| ModelOption {
                id: id.to_owned(),
                label: label.to_owned(),
                choices,
                default_choice,
            })
        }
        "boolean" => Some(ModelOption {
            id: id.to_owned(),
            label: label.to_owned(),
            choices: vec![
                ModelOptionChoice {
                    id: "off".into(),
                    label: "Off".into(),
                },
                ModelOptionChoice {
                    id: "on".into(),
                    label: "On".into(),
                },
            ],
            default_choice: if option.get("currentValue") == Some(&Value::Bool(true)) {
                "on".into()
            } else {
                "off".into()
            },
        }),
        _ => None,
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        self.spec.id
    }
    fn display_name(&self) -> &str {
        self.spec.display_name
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        self.spec.steering_mode
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        self.spec.reasoning_levels
    }

    /// The agent's own CLI, not the adapter: `claude` counts as installed even
    /// when `claude-agent-acp` would arrive via npx, and an npx-reachable
    /// adapter does NOT count when the CLI itself is missing. Explicit
    /// executables (tests, `*_EXECUTABLE` overrides) always count.
    fn installed(&self) -> bool {
        if self.executable.is_some() {
            return true;
        }
        if std::env::var_os(self.spec.env_override).is_some_and(|v| !v.is_empty()) {
            return true;
        }
        find_on_paths(self.spec.cli_executable, (self.spec.cli_extra_paths)()).is_some()
    }

    /// Devin refreshes through its native catalog command on each request.
    /// Other ACP agents use a cached session probe, with the spec's static
    /// catalog as fallback when they advertise nothing or probing fails.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_launch()?;
        if self.spec.id == HarnessId::Devin {
            let (exe, _) = self.resolve_program(false).await?;
            return self
                .devin_models
                .refresh(&exe, self.model_discovery_timeout)
                .await;
        }
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        let _probe = self.models_probe.lock().await;
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        match self.discover_models().await {
            Ok(models) if !models.is_empty() => {
                let _ = self.models_cache.set(models.clone());
                Ok(self.models_cache.get().cloned().unwrap_or(models))
            }
            Ok(_) => Ok((self.spec.models)()),
            Err(_) => Ok((self.spec.models)()),
        }
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands
            .get_or_try_init(|| self.discover_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (mut child, stderr_tail) = self.spawn_agent(Some(&request.cwd), true, &[]).await?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdout".into()))?;
        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            harness: self.spec.id,
            agent_name: self.spec.display_name,
            prompt_transform: self.spec.prompt_transform,
            effort_values: self.spec.effort_values,
            prompt_complete_extension: self.spec.prompt_complete_extension,
            prompt_stall: self.spec.prompt_stall,
            stall_hint: self.spec.stall_hint,
            sessions_root: self.sessions_root.clone(),
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            handshake_timeout: self.handshake_timeout,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    harness: HarnessId,
    agent_name: &'static str,
    prompt_complete_extension: bool,
    prompt_stall: Option<Duration>,
    stall_hint: &'static str,
    /// Sessions-root override for the subagent transcript tail (tests).
    sessions_root: Option<PathBuf>,
    prompt_transform: fn(Option<ReasoningLevel>, &str) -> String,
    effort_values: fn(Option<ReasoningLevel>, Option<&str>) -> Vec<&'static str>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    handshake_timeout: Duration,
    stderr_tail: crate::StderrTail,
}

fn initialize_params(harness: HarnessId) -> Value {
    let mut capabilities = json!({
        "fs": { "readTextFile": false, "writeTextFile": false },
        "terminal": false,
    });
    if harness == HarnessId::Devin {
        // Devin otherwise exposes only the parent's run_subagent call. This
        // unlocks lifecycle tags plus every nested message, thought, and tool
        // update, all of which DevinTracker can route. Do not advertise the
        // separate subagentControl extension: Zeron has no matching UI yet.
        capabilities["_meta"] = json!({ "cognition.ai/subagentSupport": true });
    }
    json!({
        "protocolVersion": 1,
        "clientInfo": {
            "name": "zeron",
            "title": "Zeron",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Declined: agents fall back to their own fs/terminal access, which
        // is what zeron wants — the working tree is the source of truth for
        // the diff pane, and commands belong to the agent's own sandbox.
        "clientCapabilities": capabilities,
    })
}

/// `initialize._meta.steering.supported` — the `_session/steering` extension
/// both org-maintained adapters advertise (not part of the v1 spec).
fn steering_supported(init: &Value) -> bool {
    init.get("_meta")
        .and_then(|m| m.get("steering"))
        .and_then(|s| s.get("supported"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Depth-limited scan for an `availableCommands` array anywhere in a response
/// (agents differ on where the handshake advertises them: top level, inside
/// `agentCapabilities`, or `_meta`).
fn scan_available_commands(value: &Value) -> Vec<SlashCommand> {
    fn scan(value: &Value, depth: u8) -> Option<&Value> {
        if depth == 0 {
            return None;
        }
        let obj = value.as_object()?;
        if let Some(cmds) = obj.get("availableCommands").filter(|c| c.is_array()) {
            return Some(cmds);
        }
        obj.values().find_map(|v| scan(v, depth - 1))
    }
    parse_commands(scan(value, 4))
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

/// Normalize an option/model-option id for matching across naming styles
/// (`fastMode` == `fast_mode` == `fast-mode`).
fn norm_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether a model id carries the 1M-context hint, in either spelling: the
/// display form `opus[1m]` or the SDK-id form `claude-opus-4-6-1m`.
fn context_hint_1m(id: &str) -> bool {
    id.contains("[1m]") || id.ends_with("-1m")
}

/// The id with a trailing long-context hint removed; `None` when it carries
/// none.
fn strip_context_hint(id: &str) -> Option<&str> {
    id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
}

/// A wire display name with its trailing parenthetical removed
/// ("Opus (1M context)" → "Opus") — a folded base row must not keep the
/// variant tag.
fn strip_trailing_parenthetical(name: &str) -> &str {
    match name.rfind(" (") {
        Some(at) if name.ends_with(')') => name[..at].trim_end(),
        _ => name,
    }
}

/// Pick the advertised model value for a requested model id. Agents differ in
/// what they advertise: full ids (`claude-opus-5`), SDK aliases
/// (`opus`, `sonnet`, `haiku` — the claude adapter), and long-context
/// variants in either hint spelling. Exact match first (with the 1M compose
/// when the run selects the 1M window), then a family-token fallback that
/// prefers a variant matching the requested context window.
fn pick_model_value(requested: &str, available: &[&str], context_1m: bool) -> Option<String> {
    if context_1m {
        for composed in [format!("{requested}[1m]"), format!("{requested}-1m")] {
            if available.contains(&composed.as_str()) {
                return Some(composed);
            }
        }
    }
    if available.contains(&requested) {
        return Some(requested.to_owned());
    }
    // Family fallback: "claude-opus-5" → "opus" matches "opus[1m]".
    let family = ["fable", "opus", "sonnet", "haiku", "gpt"]
        .into_iter()
        .find(|f| norm_id(requested).contains(f))?;
    let candidates: Vec<&&str> = available
        .iter()
        .filter(|v| norm_id(v).contains(family))
        .collect();
    candidates
        .iter()
        .find(|v| context_hint_1m(v) == context_1m)
        .or_else(|| candidates.first())
        .map(|v| (**v).to_owned())
}

/// Pick a first-class ACP model switch when the session uses the legacy
/// `models` state instead of a category=model config option. Grok Build 1.0.5
/// has exactly this shape and accepts the selected id through
/// `session/set_model`; treating its advertised models as config options makes
/// the picker look functional while every selection is silently ignored.
///
/// Config options remain preferred when present: org adapters can expose a
/// legacy `models` matrix alongside the canonical base-model config option.
fn first_class_model_change(
    session_response: &Value,
    requested: Option<&str>,
) -> Result<Option<String>, HarnessError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let has_model_config = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option.get("type").and_then(Value::as_str) == Some("select")
                    && option.get("category").and_then(Value::as_str) == Some("model")
            })
        });
    if has_model_config {
        return Ok(None);
    }

    let Some(models) = session_response.get("models") else {
        return Ok(None);
    };
    let available: Vec<&str> = models
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|models| models.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|model| model.get("modelId").and_then(Value::as_str))
        .collect();
    if available.is_empty() {
        return Ok(None);
    }
    if !available.contains(&requested) {
        return Err(HarnessError::Protocol(format!(
            "agent does not advertise requested model {requested}; available models: {}",
            available.join(", ")
        )));
    }
    if models.get("currentModelId").and_then(Value::as_str) == Some(requested) {
        return Ok(None);
    }
    Ok(Some(requested.to_owned()))
}

/// The `session/set_config_option` calls a session response's `configOptions`
/// warrant for this run:
/// - the requested model (category `model`; a `contextWindow: "1m"` model
///   option composes the `<model>[1m]` id first, the CLI's own convention),
/// - the effort (category `thought_level`, first advertised value from the
///   spec's preference list),
/// - any remaining `model_options` matched by normalized id — selects take
///   the choice id, booleans take `on`/`true` truthiness (fastMode, thinking).
///
/// Matched against advertised values and skipped when already current. Pure
/// so it's testable; the returned value is the request's flattened `value`
/// payload (select: `{"value": id}`, boolean: `{"type":"boolean","value": b}`).
fn config_option_sets(
    session_response: &Value,
    model: Option<&str>,
    efforts: &[&'static str],
    model_options: &serde_json::Map<String, Value>,
) -> Vec<(String, Value)> {
    let Some(options) = session_response
        .get("configOptions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let context_1m = model_options
        .get("contextWindow")
        .and_then(Value::as_str)
        .is_some_and(|w| w.eq_ignore_ascii_case("1m"));
    let mut sets = Vec::new();
    for option in options {
        let Some(config_id) = option.get("id").and_then(Value::as_str) else {
            continue;
        };
        let kind = option.get("type").and_then(Value::as_str).unwrap_or("");
        let category = option.get("category").and_then(Value::as_str);
        let current = option.get("currentValue");
        let available: Vec<&str> = option
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();

        let wanted: Option<Value> = match (kind, category) {
            ("select", Some("model")) => model
                .and_then(|m| pick_model_value(m, &available, context_1m))
                .map(Value::String),
            // Unattended parity with the retired custom adapters (claude
            // bypassPermissions / codex approvalPolicy never): pick the
            // no-prompts mode when the agent offers one. claude-agent-acp
            // calls it `bypassPermissions`, codex-acp `agent-full-access`
            // (approvalPolicy "never" + danger-full-access sandbox), Devin
            // `bypass`. Cursor instead exposes agent/plan/ask — those arrive
            // as a Traits "Mode" option and win when the run selected one.
            ("select", Some("mode")) => model_options
                .get("mode")
                .and_then(Value::as_str)
                .filter(|c| available.contains(c))
                .map(|c| Value::String(c.to_owned()))
                .or_else(|| {
                    [
                        "bypassPermissions",
                        "bypass_permissions",
                        "bypass",
                        "yolo",
                        "agent-full-access",
                        "danger-full-access",
                        "full-access",
                    ]
                    .into_iter()
                    .find(|v| available.contains(v))
                    .map(|v| Value::String(v.to_owned()))
                }),
            ("select", Some("thought_level")) => efforts
                .iter()
                .find(|c| available.contains(*c))
                .map(|c| Value::String((*c).to_owned())),
            // Everything else: best-effort match against the run's
            // model-option selections by normalized id.
            _ => model_options.iter().find_map(|(opt_id, choice)| {
                if norm_id(opt_id) != norm_id(config_id) || opt_id == "contextWindow" {
                    return None;
                }
                match kind {
                    "select" => choice
                        .as_str()
                        .filter(|c| available.contains(c))
                        .map(|c| Value::String(c.to_owned())),
                    "boolean" => {
                        let on = choice == &Value::Bool(true)
                            || choice
                                .as_str()
                                .is_some_and(|c| c.eq_ignore_ascii_case("on"));
                        Some(Value::Bool(on))
                    }
                    _ => None,
                }
            }),
        };
        if let Some(value) = wanted
            && current != Some(&value)
        {
            let payload = match value {
                Value::Bool(b) => serde_json::json!({ "type": "boolean", "value": b }),
                other => serde_json::json!({ "value": other }),
            };
            sets.push((config_id.to_owned(), payload));
        }
    }
    sets
}

/// Per-agent subagent correlation: Devin maps tagged ACP updates inline
/// (`cognition.ai/subagent_*` lifecycle on ordinary `session/update`s),
/// Grok tails child transcripts from disk (inert for agents that never
/// emit its `subagent_*` extension). Both produce the same
/// [`AgentEvent::Subagent`] contract.
enum SubagentObserver {
    Devin(DevinTracker),
    Grok(SubagentTracker),
}

impl SubagentObserver {
    fn observe(&mut self, update: &Value) {
        match self {
            SubagentObserver::Devin(_) => {}
            SubagentObserver::Grok(tracker) => tracker.observe(update),
        }
    }

    fn finish_open(&mut self, status: DoneStatus) -> Vec<AgentEvent> {
        match self {
            SubagentObserver::Devin(tracker) => tracker.finish_open(status),
            SubagentObserver::Grok(_) => Vec::new(),
        }
    }
}

/// The events of one notification, session-filtered. `session/update` maps
/// per [`map_update`]; `_x.ai/session_notification` is grok's extension
/// channel — same `{sessionId, update}` envelope, but its updates (the
/// `subagent_*` lifecycle) render nothing directly. The subagent tracker sees
/// both first (spawn/finished correlation + transcript tails); its tagged
/// events flow from its own tasks, not this return value.
fn session_update_events(
    method: &str,
    params: &Value,
    session_id: &str,
    subagents: &mut SubagentObserver,
) -> Vec<AgentEvent> {
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let update = params.get("update").unwrap_or(&Value::Null);
    match method {
        "session/update" => match subagents {
            SubagentObserver::Devin(tracker) => tracker.map(update),
            _ => {
                subagents.observe(update);
                map_update(update)
            }
        },
        "_x.ai/session_notification" => {
            subagents.observe(update);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Per-turn token usage from a settled `session/prompt` response, when the
/// adapter attaches it (tolerant of both field spellings; absent → nothing).
fn usage_from_response(res: &Result<Value, HarnessError>) -> Option<AgentEvent> {
    let resp = res.as_ref().ok()?;
    // Grok settles usage on the response `_meta` (inputTokens/outputTokens —
    // verified live, 1.0.4); adapters used a first-class `usage` object.
    let usage = resp.get("usage").or_else(|| resp.get("_meta"))?;
    let count = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k))
            .and_then(Value::as_u64)
    };
    let input = count(&["inputTokens", "input_tokens"]);
    let output = count(&["outputTokens", "output_tokens"]);
    (input.is_some() || output.is_some()).then(|| AgentEvent::Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    })
}

/// Map a finished `session/prompt` result to the run's terminal status.
fn stop_outcome(
    res: &Result<Value, HarnessError>,
    interrupted: bool,
) -> (DoneStatus, Option<String>) {
    if interrupted {
        return (DoneStatus::Interrupted, None);
    }
    match res {
        Ok(resp) => match resp.get("stopReason").and_then(Value::as_str) {
            Some("cancelled") => (DoneStatus::Interrupted, None),
            Some("refusal") => (
                DoneStatus::Errored,
                Some("The agent refused to continue.".to_owned()),
            ),
            // end_turn / max_tokens / max_turn_requests: the turn ended;
            // partial output is already in the doc.
            _ => (DoneStatus::Completed, None),
        },
        Err(e) => (DoneStatus::Errored, Some(e.to_string())),
    }
}

/// One turn: `session/prompt` whose response (the `stopReason`) ends it.
/// `prompt_id` (agents with the prompt-complete extension) rides `_meta` so
/// the `_x.ai/session/prompt_complete` notification can be matched exactly —
/// grok echoes it back (verified live, 1.0.4).
fn prompt_turn(
    client: RpcClient,
    session_id: String,
    text: String,
    prompt_id: Option<String>,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move {
        let mut params = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        });
        if let Some(id) = prompt_id {
            params["_meta"] = json!({ "promptId": id, "requestId": id });
        }
        client.request("session/prompt", params).await
    })
}

/// Answer a server→client request. Permission requests are auto-accepted with
/// the agent's preferred allow option — parity with the claude harness's
/// bypassPermissions and the codex harness's approvalPolicy "never" (zeron
/// sessions run unattended). Everything else (fs, terminal, elicitation) was
/// declined at initialize, so a stray request gets method-not-found rather
/// than wedging the agent.
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
) -> Vec<AgentEvent> {
    match method {
        "session/request_permission" => {
            let options: Vec<Value> = params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match preferred_allow_option(&options) {
                Some(option_id) => client.respond(
                    &id,
                    json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
                ),
                None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
            }
            Vec::new()
        }
        _ => {
            tracing::debug!(target: "zeron_harness::acp", "unhandled server request: {method}");
            client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
            Vec::new()
        }
    }
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// A permission request is a QUESTION (not a tool permission) when any of
/// its options lacks an allow/reject kind — that's how the agent relays
/// user-facing choices (Claude's AskUserQuestion arrives this way through
/// the adapter). Every option carrying an allow/reject kind means a real
/// tool permission, which auto-accepts (unattended parity); kinds may
/// legitimately repeat — codex sends two `allow_always` options ("Allow for
/// Session" and a prefix-rule amendment) on every exec approval.
fn is_user_question(options: &[Value]) -> bool {
    options.iter().any(|option| {
        !matches!(
            option.get("kind").and_then(Value::as_str),
            Some("allow_once" | "allow_always" | "reject_once" | "reject_always")
        )
    })
}

/// The live-run request handler: tool permissions auto-accept like
/// [`handle_server_request`], but question-shaped requests block on the
/// engine's input bridge (in a subtask so the message loop keeps flowing)
/// and answer with the option whose name matches the chosen label. A dropped
/// resolver degrades to `cancelled` — never a silent allow.
fn handle_server_request_live(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    request_input: &std::sync::Arc<RequestInputFn>,
    open_questions: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Vec<AgentEvent> {
    if method != "session/request_permission" {
        return handle_server_request(client, id, method, params);
    }
    let options: Vec<Value> = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !is_user_question(&options) {
        return handle_server_request(client, id, method, params);
    }
    let names: Vec<String> = options
        .iter()
        .map(|o| {
            o.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let question = UserInputQuestion {
        id: new_message_id(),
        header: "Agent question".into(),
        question: params
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("The agent needs your input.")
            .to_owned(),
        options: names.clone(),
        multi_select: false,
    };
    let client = client.clone();
    let request_input = std::sync::Arc::clone(request_input);
    // Pending questions block the agent — the quiet-settle must not read
    // that silence as a finished turn.
    let open_questions = std::sync::Arc::clone(open_questions);
    open_questions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let answers = (request_input)(vec![question.clone()])
            .await
            .unwrap_or_default();
        let picked = answers
            .iter()
            .find(|a| a.question_id == question.id)
            .and_then(|a| a.labels.first())
            .and_then(|label| {
                options
                    .iter()
                    .find(|o| o.get("name").and_then(Value::as_str) == Some(label.as_str()))
            })
            .and_then(|o| o.get("optionId").and_then(Value::as_str));
        match picked {
            Some(option_id) => client.respond(
                &id,
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            ),
            None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
        }
        open_questions.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
    Vec::new()
}

/// Await a setup request while draining incoming messages, so a `session/load`
/// whose replay outruns the incoming channel's capacity can't deadlock the
/// reader. Replayed `session/update`s are dropped (the doc already holds the
/// history); server requests are answered.
async fn request_draining(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    method: &'static str,
    params: Value,
) -> Result<Value, HarnessError> {
    let loading_session = matches!(method, "session/new" | "session/load");
    let requested_session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut config_updates = std::collections::HashMap::new();
    let mut handle_incoming = |inc| match inc {
        Incoming::Request { id, method, params } => {
            handle_server_request(client, id, &method, &params);
        }
        Incoming::Notification { method, params }
            if loading_session
                && method == "session/update"
                && params["update"]["sessionUpdate"] == "config_option_update" =>
        {
            if let Some(id) = params.get("sessionId").and_then(Value::as_str)
                && params["update"]["configOptions"].is_array()
            {
                config_updates.insert(id.to_owned(), params["update"]["configOptions"].clone());
            }
        }
        _ => {}
    };
    let mut fut = prompt_like_request(client.clone(), method, params);
    let res = loop {
        tokio::select! {
            res = &mut fut => break res,
            inc = incoming.recv() => match inc {
                Some(inc) => handle_incoming(inc),
                None => {
                    return Err(HarnessError::Protocol(format!(
                        "{method}: agent exited during setup"
                    )));
                }
            },
        }
    };
    // Responses resolve through the pending map, not the incoming queue, so
    // replay updates the reader forwarded BEFORE the response line may still
    // sit in the buffer — flush them now or they'd leak into the live turn.
    while let Ok(inc) = incoming.try_recv() {
        handle_incoming(inc);
    }
    // Keep config refreshes even when they race the session response. They
    // are session state, not replayed transcript, and dropping them can lose
    // the only notification advertising a newly released Devin model.
    res.map(|mut response| {
        let id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .or(requested_session.as_deref());
        if let Some(options) = id.and_then(|id| config_updates.remove(id)) {
            response["configOptions"] = options;
        }
        response
    })
}

fn prompt_like_request(
    client: RpcClient,
    method: &'static str,
    params: Value,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move { client.request(method, params).await })
}

/// Track the liveness signals the blanket quiet-settle keys on: content
/// proves the turn produced something; an open tool call or a pending
/// question proves silence is legitimate.
fn track_turn_signals(
    ev: &AgentEvent,
    content_seen: &mut bool,
    open_tools: &mut std::collections::HashSet<String>,
) {
    match ev {
        AgentEvent::TextDelta { text } if !text.is_empty() => *content_seen = true,
        AgentEvent::ToolCall { id, .. } => {
            *content_seen = true;
            open_tools.insert(id.clone());
        }
        AgentEvent::ToolResult { id, .. } => {
            open_tools.remove(id);
        }
        _ => {}
    }
}

/// A mid-turn `_session/steering` call. `idleBehavior: promptRequired`
/// covers the turn-ended race: the agent hands the text back instead of
/// firing an untracked turn.
fn steering_call_future(
    client: &RpcClient,
    session_id: &str,
    text: &str,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    let params = json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
        "_meta": { "steering": { "idleBehavior": "promptRequired" } },
    });
    prompt_like_request(client.clone(), "_session/steering", params)
}

/// The per-run event loop: one task multiplexing agent messages, the pending
/// turn, the steering mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        harness,
        agent_name,
        prompt_complete_extension,
        prompt_stall,
        stall_hint,
        sessions_root,
        prompt_transform,
        effort_values,
        interrupt_grace,
        kill_grace,
        handshake_timeout,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = std::sync::Arc::new(request_input);

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        let init = client
            .request("initialize", initialize_params(harness))
            .await?;
        let steer_ext = steering_supported(&init);
        let init_commands = scan_available_commands(&init);

        let session_params = json!({ "cwd": request.cwd, "mcpServers": [] });
        let (session_id, mut session_response) = if let Some(resume) = &request.resume {
            let mut load = session_params.clone();
            load["sessionId"] = Value::String(resume.clone());
            match request_draining(&client, &mut incoming, "session/load", load).await {
                Ok(resp) => (resume.clone(), resp),
                // A missing/foreign session falls back to a fresh one.
                Err(e) => {
                    tracing::debug!(
                        target: "zeron_harness::acp",
                        "session/load failed (starting fresh): {e}"
                    );
                    let new = request_draining(
                        &client,
                        &mut incoming,
                        "session/new",
                        session_params.clone(),
                    )
                    .await?;
                    (
                        new.get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        new,
                    )
                }
            }
        } else {
            let new =
                request_draining(&client, &mut incoming, "session/new", session_params).await?;
            (
                new.get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new,
            )
        };
        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "session/new returned no sessionId".into(),
            ));
        }
        if harness == HarnessId::Devin
            && let Some(model) = request.model.as_deref()
        {
            devin_models::wait_for_model(
                &client,
                &mut incoming,
                &session_id,
                &mut session_response,
                model,
            )
            .await?;
        }
        // ACP has had two model-selection surfaces. Newer config-option agents
        // use category=model below; Grok Build currently advertises only the
        // first-class `models` state and requires `session/set_model`. Paseo
        // follows the same split. Unlike the best-effort auxiliary options,
        // an explicit model switch is strict: prompting with a different
        // model than the picker shows is worse than surfacing the RPC error.
        if let Some(model) = first_class_model_change(&session_response, request.model.as_deref())?
        {
            request_draining(
                &client,
                &mut incoming,
                "session/set_model",
                json!({
                    "sessionId": session_id,
                    "modelId": model,
                }),
            )
            .await
            .map_err(|error| {
                HarnessError::Protocol(format!("agent rejected model switch to {model}: {error}"))
            })?;
        }
        // Apply the run's model + effort + model options through the
        // session's advertised config options. Best-effort for effort and
        // traits: a rejected auxiliary set is logged and the agent default
        // runs.
        let efforts = effort_values(request.reasoning, request.model.as_deref());
        let requested_model = request.model.as_deref();
        let options_snapshot = session_response;
        for (config_id, payload) in config_option_sets(
            &options_snapshot,
            requested_model,
            &efforts,
            &request.model_options,
        ) {
            let mut params = serde_json::Map::new();
            params.insert("sessionId".into(), session_id.clone().into());
            params.insert("configId".into(), config_id.clone().into());
            if let Some(payload) = payload.as_object() {
                for (k, v) in payload {
                    params.insert(k.clone(), v.clone());
                }
            }
            if let Err(e) = request_draining(
                &client,
                &mut incoming,
                "session/set_config_option",
                Value::Object(params),
            )
            .await
            {
                if harness == HarnessId::Devin
                    && options_snapshot["configOptions"]
                        .as_array()
                        .is_some_and(|options| {
                            options
                                .iter()
                                .any(|o| o["id"] == config_id && o["category"] == "model")
                        })
                {
                    return Err(HarnessError::Protocol(format!(
                        "Devin rejected requested model {requested_model:?}: {e}"
                    )));
                }
                tracing::debug!(
                    target: "zeron_harness::acp",
                    "session/set_config_option {config_id}={payload} rejected (agent default runs): {e}"
                );
            }
        }
        Ok::<(String, bool, Vec<SlashCommand>), HarnessError>((
            session_id,
            steer_ext,
            init_commands,
        ))
    };
    let (session_id, steer_ext, init_commands) = tokio::select! {
        res = tokio::time::timeout(handshake_timeout, setup) => {
            let res = res.unwrap_or_else(|_| {
                // A hung handshake (agent waiting on a login it can never
                // get, a wedged adapter) used to spin "Working" forever —
                // the false "thinking for 2+ minutes then nothing" class of
                // report. Bound it and say what was reached.
                Err(HarnessError::Protocol(format!(
                    "{agent_name} did not complete the ACP handshake within {}s \
                     (the agent may be waiting for a login — try running it once \
                     in a terminal)",
                    handshake_timeout.as_secs()
                )))
            });
            match res {
                Ok(v) => v,
                Err(e) => {
                    // A child that dies before the handshake used to surface only
                    // the RPC-side symptom ("transport closed") — its exit status
                    // and stderr, both already in hand, were dropped, leaving
                    // startup crashes undiagnosable (user report). When the child
                    // is already gone, give the reader task a beat to drain the
                    // pipe, then append the crash text; the Done carrying it is
                    // journaled, so the cause survives for later inspection. A
                    // still-live child (the timeout) contributes its stderr tail.
                    let error = match child.try_wait() {
                        Ok(Some(status)) => {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            format!(
                                "{e}; {}",
                                crate::crash_message(agent_name, Some(status), &stderr_tail)
                            )
                        }
                        _ => match stderr_tail.snapshot() {
                            Some(tail) => format!("{e}; stderr: {tail}"),
                            None => e.to_string(),
                        },
                    };
                    tracing::warn!(target: "zeron_harness::acp", %error, "agent setup failed");
                    let _ = event_tx
                        .send(Ok(AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some(error),
                            session_id: None,
                        }))
                        .await;
                    shutdown_child(&mut child, kill_grace).await;
                    return;
                }
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }
    if !init_commands.is_empty()
        && !send(
            &event_tx,
            AgentEvent::AvailableCommands {
                commands: init_commands,
            },
        )
        .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // Subagent correlation + transcript tails: Devin carries nested updates
    // on ACP itself; everything else gets the Grok tracker (inert without
    // Grok's subagent lifecycle extension).
    let mut subagents = if harness == HarnessId::Devin {
        SubagentObserver::Devin(DevinTracker::default())
    } else {
        SubagentObserver::Grok(SubagentTracker::new(
            session_id.clone(),
            event_tx.clone(),
            sessions_root,
        ))
    };

    // ---- main loop --------------------------------------------------------
    // Prompt-completion settlement state (the prompt-complete extension):
    // one prompt is outstanding at a time, identified by `current_prompt_id`;
    // settled ids are remembered so a STALE `prompt_complete` (a late replay
    // of an already-settled prompt) can never settle a newer turn.
    let mut prompt_seq: u64 = 0;
    let mut current_prompt_id: Option<String> = None;
    let mut completed_prompts: VecDeque<String> = VecDeque::new();
    // `ZERON_ACP_PROMPT_STALL_MS` overrides the spec's bound; 0 disables.
    let prompt_stall: Option<Duration> = match std::env::var("ZERON_ACP_PROMPT_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => prompt_stall,
    };
    let mut prompt_stall_deadline: Option<tokio::time::Instant> =
        prompt_stall.map(|d| tokio::time::Instant::now() + d);
    let mut turn: Option<BoxFuture<'static, Result<Value, HarnessError>>> = Some({
        prompt_seq += 1;
        current_prompt_id = prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
        prompt_turn(
            client.clone(),
            session_id.clone(),
            prompt_transform(request.reasoning, &request.prompt),
            current_prompt_id.clone(),
        )
    });
    // Steers waiting for the turn boundary (agents without the extension, or
    // extension steers that lost the turn-end race).
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    // The in-flight `_session/steering` call (text + response future), plus
    // followers awaiting their turn. Polled from the main select so the loop
    // keeps draining `incoming` while the agent responds — awaiting inline
    // deadlocks against a full incoming channel when the agent floods
    // updates (the reader blocks on the channel and never parses the
    // steering response).
    let mut steering_call: Option<(String, BoxFuture<'static, Result<Value, HarnessError>>)> = None;
    let mut steer_backlog: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // Starved-turn recovery (2026-08-12 stuck-Working incident): a
    // `session/prompt` sent while the agent runs a SELF-CONTINUED turn (a
    // background-task re-invocation no prompt started) starves —
    // claude-agent-acp does not track turns it did not start, so the merged
    // turn's result is never attributed to the pending prompt (reproduced
    // against 0.66.0; the prompt's TEXT still reaches the model, queued by
    // the CLI). The tell is protocol evidence, not timing: a steering call
    // answered `promptRequired`/`noRunningTurn` while OUR prompt is
    // outstanding means the adapter has no turn that could ever settle it.
    // A short grace covers the true turn-end race (its response lands within
    // milliseconds); past it, the dead prompt is closed out with a Done and
    // the queued steer is promoted to a fresh turn.
    const STARVE_GRACE: Duration = Duration::from_secs(2);
    let mut starve_deadline: Option<tokio::time::Instant> = None;
    // BLANKET dropped-reply settle, adapter-agnostic: any ACP agent whose
    // prompt response goes missing must not strand the turn. Signals that
    // exist in core ACP stand in for the adapter-specific cost frame: once
    // the turn has streamed content, every tool call it opened has resolved,
    // no permission/question round-trip is pending, and the stream has been
    // quiet past the window, the turn is settled through the same recovery
    // arm. A false settle is only PARTLY recoverable: the engine folds any
    // later output as a self-continued segment and re-arms Working, but the
    // turn is orphaned — the real response resolves a closed channel, no
    // Done ever comes, and the session strands Working until the engine's
    // quiesce watchdog parks it.
    //
    // `ZERON_ACP_QUIET_SETTLE_MS` overrides; 0 disables.
    let quiet_settle: Option<Duration> = match std::env::var("ZERON_ACP_QUIET_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_secs(30)),
    };
    let mut last_update_at = tokio::time::Instant::now();
    let mut turn_content_seen = false;
    // A steering injection makes the cost hint unsafe for the REST of the
    // turn: the adapter emits a cost-bearing usage_update for the injected
    // message itself, mid-turn, indistinguishable in shape from the
    // terminal one (verified against 0.66.0 — premature Done exactly one
    // grace after injection). Steered turns settle off their real response
    // (healthy in every trace); the engine's quiesce watchdog backstops
    // them (the quiet settle used to, before the Claude exemption above).
    let mut steered_this_turn = false;
    let mut open_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let open_questions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // PREVENTION, ahead of all the recovery above: never send a
    // `session/prompt` into a session that is visibly mid SELF-CONTINUED
    // turn — that prompt's reply is what the adapter drops (the verified
    // starve). Visibly busy = an open tool call, or stream traffic within
    // BUSY_RECENT, with no prompt of ours outstanding. The discipline is
    // Zed's, verified against the real adapter: `session/cancel` the
    // unowned turn, give it CANCEL_FLUSH to die and drain, then prompt.
    // This makes the interactive path starve-free; the settle layers below
    // remain for the notification race a client cannot see coming.
    const BUSY_RECENT: Duration = Duration::from_secs(3);
    const CANCEL_FLUSH: Duration = Duration::from_secs(2);
    let mut cancel_flush_deadline: Option<tokio::time::Instant> = None;

    'main: loop {
        tokio::select! {
            res = async { turn.as_mut().expect("guarded by if").await }, if turn.is_some() => {
                turn = None;
                starve_deadline = None;
                prompt_stall_deadline = None;
                if let Some(id) = current_prompt_id.take() {
                    completed_prompts.push_back(id);
                    while completed_prompts.len() > 32 {
                        completed_prompts.pop_front();
                    }
                }
                // Settle an in-flight `_session/steering` call BEFORE closing
                // the turn: its response rides the same stdout as the prompt
                // response, so by now it is (nearly always) already parsed —
                // the select just hadn't polled it yet. Deciding it here keeps
                // the ordering deterministic: an injection that landed in this
                // turn emits its Steered boundary now, ahead of the drained
                // tail and the Done (a Steered AFTER Done re-armed the
                // consumer with no next turn — the stranded-Working bug); a
                // rejected/unsettled call redelivers as the next turn. The
                // timeout guards the flooded-incoming edge (reader blocked on
                // a full channel never parses the response): past it the call
                // is abandoned and the steer redelivered.
                if let Some((text, mut fut)) = steering_call.take() {
                    let outcome = match tokio::time::timeout(
                        Duration::from_millis(1000),
                        &mut fut,
                    )
                    .await
                    {
                        Ok(Ok(resp)) => resp
                            .get("outcome")
                            .and_then(Value::as_str)
                            .unwrap_or("injected")
                            .to_owned(),
                        Ok(Err(_)) | Err(_) => "promptRequired".to_owned(),
                    };
                    if interrupted {
                        // Winding down; abandoned like any queued steer.
                    } else if outcome != "promptRequired" {
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    } else {
                        queued_steers.push_back(text);
                    }
                    // Followers waiting on the settled call have no live turn
                    // to inject into anymore: boundary delivery.
                    while let Some(next_text) = steer_backlog.pop_front() {
                        queued_steers.push_back(next_text);
                    }
                }
                // Updates streamed before the prompt response are already
                // queued in stdout order — fold them into the turn before
                // closing it (responses bypass the incoming queue).
                let mut consumer_gone = false;
                while let Ok(inc) = incoming.try_recv() {
                    match inc {
                        Incoming::Notification { method, params } => {
                            let events =
                                session_update_events(&method, &params, &session_id, &mut subagents);
                            for ev in events {
                                if !send(&event_tx, ev).await {
                                    consumer_gone = true;
                                    break;
                                }
                            }
                        }
                        Incoming::Request { id, method, params } => {
                            for ev in handle_server_request_live(
                                &client,
                                id,
                                &method,
                                &params,
                                &request_input,
                                &open_questions,
                            ) {
                                if !send(&event_tx, ev).await {
                                    consumer_gone = true;
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                    if consumer_gone {
                        break;
                    }
                }
                if consumer_gone {
                    break 'main;
                }
                let (prev, _next) = rotate(&mut assistant_message_id);
                if !send(
                    &event_tx,
                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                )
                .await
                {
                    break 'main;
                }
                // Per-turn token usage, when the adapter settles the prompt
                // with it (claude-agent-acp and codex-acp both do).
                if let Some(usage) = usage_from_response(&res)
                    && !send(&event_tx, usage).await
                {
                    break 'main;
                }
                let (status, error) = stop_outcome(&res, interrupted);
                done_current = true;
                if interrupted {
                    done_after_interrupt = true;
                }
                if !send(
                    &event_tx,
                    AgentEvent::Done {
                        status,
                        result: None,
                        error,
                        session_id: Some(session_id.clone()),
                    },
                )
                .await
                {
                    break 'main;
                }
                if interrupted || res.is_err() {
                    break 'main;
                }
                // Persistent session: a queued steer becomes the next turn;
                // otherwise stay alive for the mailbox — the caller owns
                // teardown (mirrors the codex harness).
                if let Some(text) = queued_steers.pop_front() {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    steered_this_turn = false;
                    open_tools.clear();
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if !steering_open {
                    break 'main;
                }
            },

            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    last_update_at = tokio::time::Instant::now();
                    // Wire traffic is a sign of life for the prompt-stall
                    // watchdog — EXCEPT session boilerplate: opencode emits
                    // available_commands_update right after session/new on
                    // every session, including ones whose provider is down
                    // (where it then retries the provider stream forever
                    // with nothing further on the wire — verified live,
                    // 1.18.18). One such frame must not disarm the watchdog
                    // for the whole turn; only turn progress counts.
                    let boilerplate = method == "session/update"
                        && matches!(
                            params
                                .get("update")
                                .and_then(|u| u.get("sessionUpdate"))
                                .and_then(Value::as_str),
                            Some("available_commands_update")
                                | Some("config_option_update")
                                | Some("current_mode_update")
                        );
                    if !boilerplate {
                        prompt_stall_deadline = None;
                    }
                    // `_x.ai/session/prompt_complete` — the AUTHORITATIVE
                    // turn end for agents advertising it (grok): the
                    // `session/prompt` RPC can hang after the turn really
                    // finished. Settle through the SAME response arm by
                    // swapping the in-flight future for the synthesized
                    // response; the abandoned RPC future drains in the
                    // background and its late result is discarded. Guards:
                    // session match, an outstanding prompt, and an exact
                    // prompt-id match (a stale replay of an already-settled
                    // prompt must never settle a newer turn; grok echoes the
                    // `_meta.promptId` we mint — verified live, 1.0.4).
                    if prompt_complete_extension
                        && method == "_x.ai/session/prompt_complete"
                        && !interrupted
                        && turn.is_some()
                        && params.get("sessionId").and_then(Value::as_str)
                            == Some(session_id.as_str())
                    {
                        let pid = params
                            .get("promptId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let stale = pid.as_deref().is_some_and(|p| {
                            completed_prompts.iter().any(|c| c == p)
                        }) || (pid.is_some() && pid != current_prompt_id);
                        if !stale {
                            let stop = params
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_owned();
                            if let Some(old) = turn.take() {
                                tokio::spawn(async move {
                                    let _ = old.await;
                                });
                            }
                            turn = Some(Box::pin(async move {
                                Ok(json!({ "stopReason": stop }))
                            }));
                        }
                    }
                    // Other notifications (other sessions, agent noise) are
                    // tolerated by design.
                    let events =
                        session_update_events(&method, &params, &session_id, &mut subagents);
                    for ev in events {
                        track_turn_signals(&ev, &mut turn_content_seen, &mut open_tools);
                        if !send(&event_tx, ev).await {
                            break 'main;
                        }
                    }
                }
                Some(Incoming::Request { id, method, params }) => {
                    prompt_stall_deadline = None;
                    for ev in handle_server_request_live(
                        &client,
                        id,
                        &method,
                        &params,
                        &request_input,
                        &open_questions,
                    ) {
                        if !send(&event_tx, ev).await {
                            break 'main;
                        }
                    }
                }
                Some(Incoming::Eof) | None => {
                    // The turn ends via a request RESPONSE, which races EOF
                    // through a different channel than notifications: an agent
                    // exiting right after its final response must read as a
                    // clean finish, not a crash. The response (if any) is
                    // already resolved by the reader before it sends Eof.
                    // Only a RESOLVED response is a clean finish; a request
                    // failed by the reader's EOF cleanup falls through to the
                    // crash-message bookkeeping below (stderr tail intact).
                    if let Some(mut fut) = turn.take()
                        && let Ok(res @ Ok(_)) =
                            tokio::time::timeout(Duration::from_millis(50), &mut fut).await
                    {
                        let (prev, _next) = rotate(&mut assistant_message_id);
                        let _ = send(
                            &event_tx,
                            AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                        )
                        .await;
                        if let Some(usage) = usage_from_response(&res) {
                            let _ = send(&event_tx, usage).await;
                        }
                        let (status, error) = stop_outcome(&res, interrupted);
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(session_id.clone()),
                            },
                        )
                        .await;
                    }
                    break 'main;
                }
            },

            res = async { steering_call.as_mut().expect("guarded by if").1.as_mut().await },
                if steering_call.is_some() =>
            {
                let (text, _) = steering_call.take().expect("guarded by if");
                let outcome = match &res {
                    Ok(resp) => resp
                        .get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("injected")
                        .to_owned(),
                    Err(e) => {
                        tracing::debug!(
                            target: "zeron_harness::acp",
                            "_session/steering failed (redelivering): {e}"
                        );
                        // Failed calls redeliver like a lost turn-end race.
                        "promptRequired".to_owned()
                    }
                };
                if interrupted {
                    // The run is winding down; the steer is abandoned like
                    // any queued steer at interrupt.
                } else if outcome != "promptRequired" {
                    // Injected into a live turn → a Steered boundary. But if
                    // the turn ended while the call was in flight, the
                    // injection was consumed by THAT turn — its output
                    // already streamed and the turn's Done already closed the
                    // segment. Emitting Steered after that Done re-armed the
                    // consumer (parked session → Working) with no next turn
                    // and no Done ever coming — the stranded-Working /
                    // eternal-timer bug. Post-turn: nothing left to do.
                    if turn.is_some() {
                        steered_this_turn = true;
                        // The injection proves the turn is LIVE: any settle
                        // deadline armed off a cost frame that raced this
                        // response is invalid evidence.
                        starve_deadline = None;
                        // Pre-injection updates can still sit in `incoming`
                        // (responses bypass that queue): drain them into the
                        // CURRENT segment first, or text the agent streamed
                        // before the injection landed folds after the split —
                        // the transcript attributes it to the reply-to-steer.
                        let mut consumer_gone = false;
                        while let Ok(inc) = incoming.try_recv() {
                            match inc {
                                Incoming::Notification { method, params } => {
                                    let events =
                                        session_update_events(&method, &params, &session_id, &mut subagents);
                                    for ev in events {
                                        if !send(&event_tx, ev).await {
                                            consumer_gone = true;
                                            break;
                                        }
                                    }
                                }
                                Incoming::Request { id, method, params } => {
                                    for ev in handle_server_request_live(
                                        &client,
                                        id,
                                        &method,
                                        &params,
                                        &request_input,
                                        &open_questions,
                                    ) {
                                        if !send(&event_tx, ev).await {
                                            consumer_gone = true;
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if consumer_gone {
                                break;
                            }
                        }
                        if consumer_gone {
                            break 'main;
                        }
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    }
                } else if turn.is_some() {
                    // Raced the turn end: redeliver at the boundary the
                    // loop is about to hit. `noRunningTurn` is stronger —
                    // the adapter says nothing is running while our prompt
                    // is still outstanding: the starved-turn signature. Arm
                    // the grace deadline; if the prompt's response does not
                    // land first, the recovery arm below settles the dead
                    // turn and promotes this steer.
                    if res
                        .as_ref()
                        .ok()
                        .and_then(|r| r.get("reason"))
                        .and_then(Value::as_str)
                        == Some("noRunningTurn")
                    {
                        tracing::warn!(
                            target: "zeron_harness::acp",
                            "steering answered noRunningTurn with a prompt \
                             outstanding; arming starved-turn recovery"
                        );
                        starve_deadline =
                            Some(tokio::time::Instant::now() + STARVE_GRACE);
                    }
                    queued_steers.push_back(text);
                } else {
                    // The turn ended while the call was in flight and its
                    // boundary already passed — the steer becomes the next
                    // turn directly.
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    steered_this_turn = false;
                    open_tools.clear();
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                }
                while let Some(next_text) = steer_backlog.pop_front() {
                    if turn.is_some() && !interrupted {
                        let fut = steering_call_future(&client, &session_id, &next_text);
                        steering_call = Some((next_text, fut));
                        break;
                    }
                    // No live turn to inject into: boundary delivery.
                    queued_steers.push_back(next_text);
                }
            },

            // Busy-session cancel flushed (see BUSY_RECENT/CANCEL_FLUSH
            // above): the unowned self-continued turn had its cancel and a
            // drain window; the queued steer becomes a fresh prompt on a
            // now-idle agent.
            _ = tokio::time::sleep_until(
                cancel_flush_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if cancel_flush_deadline.is_some() && !interrupted => {
                cancel_flush_deadline = None;
                if turn.is_none()
                    && let Some(text) = queued_steers.pop_front()
                {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    steered_this_turn = false;
                    open_tools.clear();
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if turn.is_none() && !steering_open {
                    // Mailbox closed while the flush waited: nothing left.
                    break 'main;
                }
            },

            // BLANKET quiet settle (see `quiet_settle` above), adapter-
            // agnostic: content streamed, every tool resolved, no question
            // pending, stream quiet past the window with the prompt still
            // unsettled. Feeds the recovery arm below by expiring its
            // deadline — one settle path for all three evidence sources.
            _ = tokio::time::sleep_until(
                last_update_at + quiet_settle.unwrap_or_default()
            ), if quiet_settle.is_some()
                && starve_deadline.is_none()
                && turn.is_some()
                && !interrupted
                && turn_content_seen
                && open_tools.is_empty()
                && open_questions.load(std::sync::atomic::Ordering::SeqCst) == 0 =>
            {
                tracing::warn!(
                    target: "zeron_harness::acp",
                    quiet_ms = quiet_settle.unwrap_or_default().as_millis() as u64,
                    "turn quiet past the settle window with completed output; \
                     treating the prompt response as dropped"
                );
                starve_deadline = Some(tokio::time::Instant::now());
            },

            // Starved-turn recovery: the grace elapsed with the prompt still
            // unsettled after turn-end evidence — the turn's terminal cost
            // frame (COST_HINT_GRACE, ~immediate), a steering call answered
            // noRunningTurn (STARVE_GRACE), or the blanket quiet settle
            // above. Close the dead turn out
            // with a Done — its output already streamed as session/updates
            // and its text was delivered via the CLI's own queue — then
            // promote any queued steer to a fresh prompt, which settles
            // normally on a now-idle agent (verified against the real
            // adapter).
            _ = tokio::time::sleep_until(
                starve_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if starve_deadline.is_some() && turn.is_some() && !interrupted => {
                starve_deadline = None;
                tracing::warn!(
                    target: "zeron_harness::acp",
                    "prompt response missing past turn-end evidence; settling \
                     the dead turn (and promoting any queued steer)"
                );
                // Drop the dead future: a response that somehow arrives later
                // resolves a closed channel harmlessly.
                turn = None;
                let (prev, _next) = rotate(&mut assistant_message_id);
                if !send(
                    &event_tx,
                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                )
                .await
                {
                    break 'main;
                }
                done_current = true;
                if !send(
                    &event_tx,
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some(session_id.clone()),
                    },
                )
                .await
                {
                    break 'main;
                }
                if let Some(text) = queued_steers.pop_front() {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn_content_seen = false;
                    steered_this_turn = false;
                    open_tools.clear();
                    last_update_at = tokio::time::Instant::now();
                    prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                } else if !steering_open {
                    // Mirror the normal turn-settled exit: mailbox closed
                    // and nothing left to run — the session is over.
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    // Same transform as the initial prompt: Claude's
                    // Ultrathink prefix rides every steer too.
                    let text = prompt_transform(request.reasoning, &msg.prompt);
                    if turn.is_none() && cancel_flush_deadline.is_some() {
                        // A busy-session cancel is already in flight: this
                        // steer lines up behind it and dispatches at flush.
                        queued_steers.push_back(text);
                    } else if turn.is_none()
                        && (!open_tools.is_empty()
                            || last_update_at.elapsed() < BUSY_RECENT)
                    {
                        // Mid self-continued turn (see BUSY_RECENT above):
                        // cancel it rather than prompt into the starve.
                        //
                        // Claude skips this branch ON PURPOSE and prompts
                        // straight in — its NATIVE semantics: the CLI queues
                        // the message and folds it into the running turn (no
                        // work lost, verified from live session data). The
                        // adapter drops that prompt's reply, and the
                        // cost-frame settle reconstructs it ~1s after the
                        // merged turn really ends. Only adapters with no
                        // verified turn-end frame pay the cancel.
                        tracing::info!(
                            target: "zeron_harness::acp",
                            "steer into a self-continuing session; cancelling \
                             the unowned turn before prompting"
                        );
                        client.notify(
                            "session/cancel",
                            Some(json!({ "sessionId": session_id })),
                        );
                        queued_steers.push_back(text);
                        cancel_flush_deadline =
                            Some(tokio::time::Instant::now() + CANCEL_FLUSH);
                    } else if turn.is_none() {
                        // Idle between turns: a steer is simply the next turn.
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        done_current = false;
                        turn_content_seen = false;
                        steered_this_turn = false;
                        open_tools.clear();
                        last_update_at = tokio::time::Instant::now();
                        prompt_seq += 1;
                    current_prompt_id =
                        prompt_complete_extension.then(|| format!("zeron-p{prompt_seq}"));
                    prompt_stall_deadline =
                        prompt_stall.map(|d| tokio::time::Instant::now() + d);
                    turn = Some(prompt_turn(
                        client.clone(),
                        session_id.clone(),
                        text,
                        current_prompt_id.clone(),
                    ));
                    } else if steer_ext {
                        // Mid-turn injection via the `_session/steering`
                        // extension: start the call, resolved by its own
                        // select branch. One call in flight at a time;
                        // followers wait in the backlog.
                        if steering_call.is_some() {
                            steer_backlog.push_back(text);
                        } else {
                            let fut = steering_call_future(&client, &session_id, &text);
                            steering_call = Some((text, fut));
                        }
                    } else {
                        // No extension (Grok today): turn-boundary delivery.
                        queued_steers.push_back(text);
                    }
                }
                None => {
                    steering_open = false;
                    if turn.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if turn.is_some() {
                    client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                    // Escalate if the agent doesn't wind down (stopReason
                    // "cancelled") within the grace periods.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to cancel — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            // Prompt-stall watchdog: a prompt was sent and NOTHING has come
            // back on the wire at all — no queue bookkeeping, no updates, no
            // requests. Healthy grok acknowledges within milliseconds; total
            // silence past the bound is a wedged agent (stale shared leader,
            // launch-time update check). Surface a visible error instead of
            // indefinite Working.
            _ = tokio::time::sleep_until(
                prompt_stall_deadline.unwrap_or_else(tokio::time::Instant::now),
            ), if prompt_stall_deadline.is_some() && turn.is_some() && !interrupted => {
                prompt_stall_deadline = None;
                let _ = send(
                    &event_tx,
                    AgentEvent::Error {
                        message: format!(
                            "{agent_name} did not respond to the prompt at all \
                             (no wire activity for {}s). {}",
                            prompt_stall.map(|d| d.as_secs()).unwrap_or(0),
                            stall_hint,
                        ),
                    },
                )
                .await;
                done_current = true;
                let _ = send(
                    &event_tx,
                    AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(format!(
                            "{agent_name} is unresponsive — the run was closed."
                        )),
                        session_id: Some(session_id.clone()),
                    },
                )
                .await;
                break 'main;
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // A vanished Devin process cannot send subagent_completed. Settle every
    // open nested transcript before the parent's terminal Done.
    let subagent_status = if interrupted {
        DoneStatus::Interrupted
    } else {
        DoneStatus::Errored
    };
    for event in subagents.finish_open(subagent_status) {
        if !send(&event_tx, event).await {
            break;
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
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child killed mid-turn must not read as a silent success.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(agent_name, status, &stderr_tail)),
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    // Escalation dies BEFORE the child is reaped: after `shutdown_child`
    // waits the pid, a still-armed SIGTERM/SIGKILL timer would fire at a
    // freed (reusable) pid.
    if let Some(handle) = escalation {
        handle.abort();
    }
    shutdown_child(&mut child, kill_grace).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devin_initialize_enables_only_the_supported_subagent_stream() {
        let devin = initialize_params(HarnessId::Devin);
        let meta = &devin["clientCapabilities"]["_meta"];
        assert_eq!(meta["cognition.ai/subagentSupport"], true);
        assert!(meta.get("cognition.ai/subagentControl").is_none());

        let grok = initialize_params(HarnessId::Grok);
        assert!(grok["clientCapabilities"].get("_meta").is_none());
    }

    #[test]
    fn steering_capability_reads_initialize_meta() {
        assert!(steering_supported(&json!({
            "protocolVersion": 1,
            "_meta": { "steering": { "supported": true } },
        })));
        assert!(!steering_supported(&json!({ "protocolVersion": 1 })));
        assert!(!steering_supported(&json!({
            "_meta": { "steering": { "supported": false } },
        })));
    }

    #[test]
    fn config_option_sets_map_model_effort_and_model_options() {
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "claude-sonnet-5",
                    "options": [
                        { "value": "claude-sonnet-5", "name": "Sonnet 5" },
                        { "value": "claude-opus-5", "name": "Opus 5" },
                        { "value": "claude-opus-5[1m]", "name": "Opus 5 (1M)" },
                    ],
                },
                {
                    "id": "effort",
                    "name": "Reasoning effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "high",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                        { "value": "max", "name": "Max" },
                    ],
                },
                {
                    "id": "fast_mode",
                    "name": "Fast mode",
                    "category": "model_config",
                    "type": "boolean",
                    "currentValue": false,
                },
            ],
        });
        let no_opts = serde_json::Map::new();
        // Model switch + effort preference list; fastMode untouched without a
        // model-option selection.
        assert_eq!(
            config_option_sets(&response, Some("claude-opus-5"), &["medium"], &no_opts),
            vec![
                ("model".to_owned(), json!({ "value": "claude-opus-5" })),
                ("effort".to_owned(), json!({ "value": "medium" })),
            ]
        );
        // Effort preference order: first ADVERTISED candidate wins.
        assert_eq!(
            config_option_sets(&response, None, &["xhigh", "max"], &no_opts),
            vec![("effort".to_owned(), json!({ "value": "max" }))]
        );
        // contextWindow=1m composes the [1m] model id; fastMode=on matches the
        // boolean option across naming styles (fastMode vs fast_mode).
        let mut opts = serde_json::Map::new();
        opts.insert("contextWindow".into(), json!("1m"));
        opts.insert("fastMode".into(), json!("on"));
        assert_eq!(
            config_option_sets(&response, Some("claude-opus-5"), &["high"], &opts),
            vec![
                ("model".to_owned(), json!({ "value": "claude-opus-5[1m]" })),
                (
                    "fast_mode".to_owned(),
                    json!({ "type": "boolean", "value": true })
                ),
            ]
        );
        // Already-current values and unadvertised models set nothing.
        assert_eq!(
            config_option_sets(&response, Some("claude-sonnet-5"), &["high"], &no_opts),
            Vec::new()
        );
        assert_eq!(
            config_option_sets(&response, Some("gpt-5.6-sol"), &[], &no_opts),
            Vec::new()
        );
        // No configOptions advertised → nothing to set.
        assert_eq!(
            config_option_sets(&json!({"sessionId": "s"}), Some("x"), &["high"], &no_opts),
            Vec::new()
        );
    }

    #[test]
    fn first_class_models_use_session_set_model_without_config_option() {
        let response = json!({
            "models": {
                "currentModelId": "grok-4.6",
                "availableModels": [
                    { "modelId": "grok-4.6", "name": "Grok 4.6" },
                    { "modelId": "grok-4.5", "name": "Grok 4.5" },
                ],
            },
        });
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.5")).unwrap(),
            Some("grok-4.5".into())
        );
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.6")).unwrap(),
            None
        );
        assert!(first_class_model_change(&response, Some("unknown")).is_err());
    }

    #[test]
    fn model_config_option_takes_precedence_over_legacy_models_state() {
        let response = json!({
            "models": {
                "currentModelId": "gpt-5.6-sol low",
                "availableModels": [
                    { "modelId": "gpt-5.6-sol low", "name": "GPT-5.6-Sol (low)" },
                ],
            },
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "gpt-5.6-sol",
                "options": [
                    { "value": "gpt-5.6-sol", "name": "GPT-5.6-Sol" },
                    { "value": "gpt-5.6-terra", "name": "GPT-5.6-Terra" },
                ],
            }],
        });
        assert_eq!(
            first_class_model_change(&response, Some("gpt-5.6-terra")).unwrap(),
            None
        );
    }

    #[test]
    fn models_prefer_the_model_config_option_over_legacy_available_models() {
        // codex-acp shape: the legacy models state enumerates model × effort,
        // the config options carry base ids + a separate thought_level select.
        let response = json!({
            "sessionId": "s-1",
            "models": {
                "currentModelId": "gpt-5.6-sol low",
                "availableModels": [
                    { "modelId": "gpt-5.6-sol low", "name": "GPT-5.6-Sol (low)" },
                    { "modelId": "gpt-5.6-sol medium", "name": "GPT-5.6-Sol (medium)" },
                    { "modelId": "gpt-5.6-terra low", "name": "GPT-5.6-Terra (low)" },
                ],
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "agent",
                    "options": [
                        { "value": "read-only", "name": "Read Only" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "agent-full-access", "name": "Agent (full access)" },
                    ],
                },
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gpt-5.6-sol",
                    "options": [
                        { "value": "gpt-5.6-sol", "name": "GPT-5.6-Sol", "description": "Frontier" },
                        { "value": "gpt-5.6-terra", "name": "GPT-5.6-Terra" },
                    ],
                },
                {
                    "id": "reasoning_effort",
                    "name": "Reasoning effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "medium",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                    ],
                },
                {
                    "id": "fast-mode",
                    "name": "Fast mode",
                    "category": "model_config",
                    "type": "select",
                    "currentValue": "off",
                    "options": [
                        { "value": "off", "name": "Off" },
                        { "value": "on", "name": "On" },
                    ],
                },
            ],
        });
        let models = models_from_session(&response, &crate::codex::catalog::static_models());
        // Two base models — never one row per effort variant.
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-5.6-terra"]
        );
        // Catalog match keeps the curated per-model ladder; wire wins on
        // label/description.
        assert_eq!(models[0].label, "GPT-5.6-Sol");
        assert_eq!(models[0].description.as_deref(), Some("Frontier"));
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        // Wire config options become traits; mode/model/thought_level do not.
        assert_eq!(
            models[0]
                .options
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fast-mode"]
        );
        assert_eq!(models[0].options[0].default_choice, "off");
    }

    #[test]
    fn model_1m_variants_collapse_into_a_context_window_trait() {
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "claude-sonnet-5",
                    "options": [
                        { "value": "claude-sonnet-5", "name": "Sonnet 5" },
                        { "value": "claude-sonnet-5[1m]", "name": "Sonnet 5 (1M)" },
                        // SDK-id hint spelling collapses too.
                        { "value": "claude-opus-4-6", "name": "Opus 4.6" },
                        { "value": "claude-opus-4-6-1m", "name": "Opus 4.6 (1M)" },
                        { "value": "claude-haiku-4-5", "name": "Haiku 4.5" },
                    ],
                },
            ],
        });
        let models = models_from_session(&response, &[]);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["claude-sonnet-5", "claude-opus-4-6", "claude-haiku-4-5"]
        );
        assert!(models[0].options.iter().any(|o| o.id == "contextWindow"));
        assert!(models[1].options.iter().any(|o| o.id == "contextWindow"));
        assert!(models[2].options.is_empty());
    }

    #[test]
    fn default_alias_drops_and_orphan_1m_variants_fold_to_their_base() {
        // The real claude adapter advertises a `default` alias row plus
        // `opus[1m]` with NO bare `opus` (the CLI pins the 1M window).
        // Both made the picker read like a settings dump (user report):
        // `default` duplicates a real model, and the orphan 1M variant now
        // presents AS its base model with the Context Window trait pinned
        // to 1M.
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "claude-fable-5[1m]",
                "options": [
                    { "value": "default", "name": "Default (recommended)" },
                    { "value": "opus[1m]", "name": "Opus (1M context)" },
                    { "value": "claude-fable-5[1m]", "name": "Fable 5" },
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "haiku", "name": "Haiku" },
                ],
            }],
        });
        let models = models_from_session(&response, &[]);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "claude-fable-5", "sonnet", "haiku"]
        );
        // The folded rows keep a de-parenthesized wire name (no catalog
        // here) and carry the 1M-pinned window trait.
        assert_eq!(models[0].label, "Opus");
        let window = models[0].options.iter().find(|o| o.id == "contextWindow");
        assert_eq!(window.map(|o| o.default_choice.as_str()), Some("1m"));
        assert!(
            models[1]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        // The bare aliases stay untouched.
        assert!(models[2].options.is_empty());
        assert!(models[3].options.is_empty());
    }

    #[test]
    fn claude_aliases_enrich_from_the_curated_catalog() {
        // Same wire shape, WITH the claude catalog: bare aliases pick up the
        // flagship row's curated label/description/ladder, versioned ids
        // keep their wire name.
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "default",
                "options": [
                    { "value": "default", "name": "Default (recommended)" },
                    { "value": "opus[1m]", "name": "Opus (1M context)" },
                    { "value": "fable", "name": "Fable" },
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "haiku", "name": "Haiku" },
                ],
            }],
        });
        let models = models_from_session(&response, &crate::claude::catalog::static_models());
        assert_eq!(
            models.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec!["Opus 5", "Fable 5.1", "Sonnet 5", "Haiku 4.5"]
        );
        // The alias rows carry the catalog's per-model ladders.
        assert!(
            models[1]
                .reasoning_levels
                .contains(&ReasoningLevel::Ultracode)
        );
        assert!(models[3].reasoning_levels.is_empty());
        // Versioned ids never fuzzy-match: a foreign id passes through.
        let foreign = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model", "category": "model", "type": "select",
                "options": [{ "value": "claude-opus-9-mini", "name": "Opus 9 Mini" }],
            }],
        });
        let models = models_from_session(&foreign, &crate::claude::catalog::static_models());
        assert_eq!(models[0].label, "Opus 9 Mini");
    }

    #[test]
    fn models_fall_back_to_legacy_state_with_catalog_options() {
        let response = json!({
            "sessionId": "s-1",
            "models": {
                "availableModels": [
                    { "modelId": "gpt-5.6-sol", "name": "GPT-5.6-Sol" },
                    { "modelId": "gpt-x", "name": "GPT-X" },
                ],
            },
        });
        let models = models_from_session(&response, &crate::codex::catalog::static_models());
        assert_eq!(models.len(), 2);
        // Catalog-matched id keeps the curated options on the legacy path…
        assert!(models[0].options.iter().any(|o| o.id == "serviceTier"));
        // …unknown ids get none.
        assert!(models[1].options.is_empty());
    }

    #[test]
    fn codex_exec_approval_options_are_not_a_question() {
        // codex-acp's real exec-approval shape: two allow_always entries (the
        // session allow + a prefix-rule amendment). Must auto-accept.
        let options = vec![
            json!({ "optionId": "allow_once", "name": "Allow Once", "kind": "allow_once" }),
            json!({ "optionId": "allow_always", "name": "Allow for Session", "kind": "allow_always" }),
            json!({ "optionId": "allow_prefix", "name": "Allow Commands Starting With `cargo test`", "kind": "allow_always" }),
            json!({ "optionId": "reject", "name": "Reject", "kind": "reject_once" }),
        ];
        assert!(!is_user_question(&options));
        // AskUserQuestion relays choices without allow/reject kinds.
        let question = vec![
            json!({ "optionId": "a", "name": "Blue" }),
            json!({ "optionId": "b", "name": "Green" }),
        ];
        assert!(is_user_question(&question));
        let mixed = vec![
            json!({ "optionId": "a", "name": "Proceed", "kind": "allow_once" }),
            json!({ "optionId": "b", "name": "Другое", "kind": "other" }),
        ];
        assert!(is_user_question(&mixed));
    }

    #[test]
    fn mode_config_option_prefers_a_no_prompt_mode_per_adapter_naming() {
        let codex = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "mode",
                "category": "mode",
                "type": "select",
                "currentValue": "agent",
                "options": [
                    { "value": "read-only" },
                    { "value": "agent" },
                    { "value": "agent-full-access" },
                ],
            }],
        });
        let no_opts = serde_json::Map::new();
        assert_eq!(
            config_option_sets(&codex, None, &[], &no_opts),
            vec![("mode".to_owned(), json!({ "value": "agent-full-access" }))]
        );
    }

    #[test]
    fn command_scan_finds_nested_advertisements() {
        let init = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "_meta": {
                    "availableCommands": [
                        { "name": "compact", "description": "Compact the session" },
                    ],
                },
            },
        });
        let commands = scan_available_commands(&init);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "compact");
        assert!(scan_available_commands(&json!({ "protocolVersion": 1 })).is_empty());
    }
}
