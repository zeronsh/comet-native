//! Synced entity rows (workspace doc) and local projections.
//!
//! In comet these were Orbit-synced Postgres rows; in comet-native they live in the per-org
//! workspace Loro doc (see ARCHITECTURE.md §2.2) with the same field surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{HarnessId, ReasoningLevel, SandboxLevel};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// First registration time (comet devices.created_at — the Devices page
    /// "Added …" fragment). Optional so pre-existing docs stay readable.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A synced (device, folder) pair — the unit of organization in the sidebar.
/// Sessions belong to exactly one space; the space fixes their host device and
/// base cwd. Folders need not be git repos: `git_detected` is stamped by the
/// owning device (SpacesSync) and gates branch pickers / the diff sidebar on
/// every device without an RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    /// Owning device — fixed at create, immutable.
    pub device_id: String,
    /// Absolute folder path on the owning device.
    pub path: String,
    /// User rename; absent ⇒ display = basename(path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Owner-stamped: is `path` inside a git work tree?
    #[serde(default)]
    pub git_detected: bool,
    /// Owner-stamped freshness timestamp of the last git check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_checked_at: Option<DateTime<Utc>>,
    /// Owner-stamped when git: canonical checkout identity of the space root
    /// (sha256(deviceId ‖ NUL ‖ git_dir)) — diff grouping key for root sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Space {
    /// Name override, else basename(path), else the path itself.
    /// Lives here (proto) so UI and engine agree on the derivation.
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.as_deref()
            && !name.trim().is_empty()
        {
            return name;
        }
        let trimmed = self.path.trim_end_matches(['/', '\\']);
        trimmed
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatConfig {
    pub harness: HarnessId,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub sandbox: SandboxLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    /// Owning (host) device.
    pub device_id: String,
    pub title: Option<String>,
    pub archived: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Canonical id of the repo checkout/worktree this chat operates in.
    pub checkout_id: Option<String>,
    pub config: Option<ChatConfig>,
    pub last_message_preview: Option<String>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Harness-native session id of the chat's latest run — engine-owned resume
    /// continuity across engine restarts (comet's `chats.harness_session_id`,
    /// written via `orbit.setChatHarnessSession`). Empty string = explicit
    /// "do not resume" tombstone after a rejected resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    /// Cwd the harness session was created under. Harness session stores are
    /// cwd-scoped (claude keys conversations by project directory), so resume
    /// is only injected when the next run launches from the same cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_cwd: Option<String>,
    /// The space this chat belongs to. Invariant: `Some` for every UI-created
    /// chat; rows with a missing/dangling space id are not rendered (the host
    /// device's repair sweep deletes its own danglers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// Synced LWW seen marker — compared against `last_message_at` to derive
    /// the "completed (finished but unseen)" indicator. Reading a chat on any
    /// device clears the badge everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Chat {
    /// True when the chat has activity the user hasn't seen on any device.
    pub fn unseen(&self) -> bool {
        match (self.last_message_at, self.last_seen_at) {
            (Some(msg), Some(seen)) => msg > seen,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

/// Display status for a chat row/tab: the four user-facing states plus a
/// distinct Errored. Derived — never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatIndicator {
    Working,
    AwaitingInput,
    Errored,
    /// Finished running (or errored out) but not seen yet on any device.
    Completed,
    Idle,
}

/// Derive the display status. `live` must already be staleness-gated by the
/// caller (the UI's 45s window) — pass `None` for a stale/absent session row.
pub fn chat_indicator(chat: &Chat, live: Option<&Session>) -> ChatIndicator {
    match live.map(|s| s.status) {
        Some(SessionStatus::Working) => ChatIndicator::Working,
        Some(SessionStatus::AwaitingInput) => ChatIndicator::AwaitingInput,
        Some(SessionStatus::Errored) if chat.unseen() => ChatIndicator::Errored,
        _ if chat.unseen() => ChatIndicator::Completed,
        _ => ChatIndicator::Idle,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Idle,
    Working,
    AwaitingInput,
    Errored,
}

/// Live run status for a chat — drives the Working indicator and sidebar status dots.
/// Staleness-checked client-side against `updated_at` so a crashed backend never shows
/// an eternal "Working".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub chat_id: String,
    pub device_id: String,
    pub status: SessionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub path: String,
    pub name: String,
    pub default_branch: Option<String>,
}

/// One row of `ListRefs`: a branch plus its checkout state — whether it is
/// the repo's current (main-checkout) branch and whether it is materialized
/// as a linked worktree. Drives the composer's ref picker (`current` /
/// `worktree` tags) and the checkout-kind selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRef {
    pub name: String,
    /// Checked out in the repo's MAIN folder right now.
    #[serde(default)]
    pub current: bool,
    /// Path of the linked worktree this branch is checked out in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub repo_path: String,
    pub path: String,
    pub branch: String,
    /// Generated worktree folder name (`comet/<name>` is its branch).
    #[serde(default)]
    pub name: String,
    /// Canonical checkout identity (device-scoped hash of the git dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderListing {
    pub path: String,
    pub entries: Vec<FolderEntry>,
    /// True when the listing hit the entry cap.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileSummary {
    pub path: String,
    /// Previous path for renames/copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    #[serde(default)]
    pub binary: bool,
}

/// Working-tree diff for a checkout — latest-only sidecar, 3MiB patch cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiff {
    pub checkout_id: String,
    pub device_id: String,
    pub cwd: String,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    /// True when the patch was truncated at the byte cap ("Partial snapshot").
    pub truncated: bool,
    pub checksum: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum AuthState {
    SignedOut,
    NeedsOrganization {
        user: UserProfile,
    },
    #[serde(rename_all = "camelCase")]
    SignedIn {
        user: UserProfile,
        org_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccount {
    pub id: String,
    pub harness: HarnessId,
    pub email: Option<String>,
    pub plan_label: Option<String>,
    pub active: bool,
    #[serde(default)]
    pub usage_windows: Vec<AgentUsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// How the CLI is signed in (`oauth` account vs raw `api-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_kind: Option<AgentAuthKind>,
    /// False for a live login whose credentials we could not read (e.g. macOS
    /// Keychain denied) — shown, but not re-activatable.
    #[serde(default)]
    pub switchable: bool,
    /// Epoch millis of the slot's last snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentAuthKind {
    Oauth,
    ApiKey,
}

/// Everything the Accounts settings page renders, rebuilt after every mutation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountsSnapshot {
    pub accounts: Vec<AgentAccount>,
    pub warnings: Vec<AgentAccountWarning>,
}

/// A per-harness detection warning (e.g. Keychain denied reading the live login).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountWarning {
    pub harness: HarnessId,
    pub message: String,
}

/// `StartAgentLogin` reply: open `url`, then either paste the code back
/// (`CompleteAgentLogin`) or poll until the browser flow lands (`PollAgentLogin`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLoginStart {
    pub login_id: String,
    pub url: String,
    pub mode: AgentLoginMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLoginMode {
    /// Claude: the user pastes the OAuth code back into the app.
    PasteCode,
    /// Codex: the CLI's loopback callback completes in the browser; poll until done.
    Browser,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLoginPoll {
    pub status: AgentLoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentLoginStatus {
    Pending,
    Done,
    Error,
}

/// CLI plan rate-limit window (accounts settings meters) — NOT app token accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsageWindow {
    pub label: String,
    /// 0.0..=1.0
    pub used_fraction: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

/// An open PTY session on the owning device (`OpenTerminal` reply).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSession {
    pub id: String,
    pub cwd: String,
    /// Shell basename (`zsh`, `bash`, …) for the tab label.
    pub shell: String,
}

/// One `SubscribeTerminal` stream item. `seq` is a per-terminal monotonic counter
/// used for replay resumption (`afterSeq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TerminalEvent {
    /// Output chunk; `data` is base64 (PTY output is raw bytes, not valid UTF-8).
    Data { seq: u64, data: String },
    #[serde(rename_all = "camelCase")]
    Exit {
        seq: u64,
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
}
