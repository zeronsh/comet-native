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
