//! AgentAccounts — the Claude Code / Codex CLI logins on this device
//! (feature-inventory §3.7 "Agent accounts"; port of comet's `agent-accounts.ts`).
//!
//! Each CLI stores exactly one live login:
//!
//! - **Claude Code** — credentials in `~/.claude/.credentials.json`
//!   (`$CLAUDE_CONFIG_DIR` relocates the dir) or, on macOS, the Keychain item
//!   `Claude Code-credentials`; the account identity (`oauthAccount`, `userID`)
//!   lives in `~/.claude.json`.
//! - **Codex** — `$CODEX_HOME/auth.json` (default `~/.codex`): a ChatGPT OAuth
//!   token set (identity inside the `id_token` JWT) or a raw API key.
//!
//! Claude-swap mechanics:
//!
//! 1. **Detect** the live login of each CLI and auto-snapshot it into a slot
//!    under `{data_dir}/agent-accounts/{harness}/{slotId}.json` — the current
//!    session is always backed up before any swap, and refreshed tokens stay
//!    current.
//! 2. **Swap** (`activate`): overwrite the CLI's credential store (and, for
//!    Claude, merge the identity back into `~/.claude.json`) with a saved slot.
//! 3. **Add** (`start_login`…): drive an OAuth flow for a NEW account without
//!    touching the live one. Claude uses the public PKCE code flow (paste-code);
//!    Codex spawns `codex login` against a throwaway `CODEX_HOME` and polls
//!    until its loopback callback lands.
//!
//! Usage probes: both providers expose the rate-limit view their own CLIs render
//! (`/usage` in Claude Code, `/status` in Codex). Unlike comet (fetch on every
//! list, 60s cache), native only hits the network when `force_usage` is set —
//! the default list stays offline-fast and deterministic; the UI passes
//! `forceUsage` on page mount/refresh. Cached results (60s TTL) are served to
//! non-forced lists in between.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use comet_proto::{
    AgentAccount, AgentAccountWarning, AgentAccountsSnapshot, AgentAuthKind, AgentLoginMode,
    AgentLoginPoll, AgentLoginStart, AgentLoginStatus, AgentUsageWindow, HarnessId,
};

use crate::repos::home_dir;
use crate::{EngineError, new_id, now_ms};

// Claude Code's public OAuth client (the one the CLI itself uses for the manual
// "paste the code" flow — no secret involved, PKCE carries the proof).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference";
const CLAUDE_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const USAGE_TTL: Duration = Duration::from_secs(60);
/// An abandoned login flow (dialog dismissed without Cancel) is reaped past this.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Filesystem knobs — env-resolved in production ([`AgentAccountsConfig::detect`]),
/// explicit in tests.
#[derive(Debug, Clone)]
pub struct AgentAccountsConfig {
    /// Engine data dir; slots live under `{data_dir}/agent-accounts/`.
    pub data_dir: PathBuf,
    /// Claude config dir (`$CLAUDE_CONFIG_DIR` or `~/.claude`) — holds `.credentials.json`.
    pub claude_config_dir: PathBuf,
    /// Claude identity file (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`).
    pub claude_config_file: PathBuf,
    /// Codex home (`$CODEX_HOME` or `~/.codex`) — holds `auth.json`.
    pub codex_home: PathBuf,
}

impl AgentAccountsConfig {
    /// Production resolution: `CLAUDE_CONFIG_DIR` relocates both the Claude config
    /// json and the credentials file; `CODEX_HOME` relocates the Codex auth file.
    pub fn detect(data_dir: &Path) -> Self {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        let claude_dir = env_dir("CLAUDE_CONFIG_DIR");
        let claude_config_file = match &claude_dir {
            Some(dir) => dir.join(".claude.json"),
            None => home_dir().join(".claude.json"),
        };
        Self {
            data_dir: data_dir.to_path_buf(),
            claude_config_dir: claude_dir.unwrap_or_else(|| home_dir().join(".claude")),
            claude_config_file,
            codex_home: env_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex")),
        }
    }

    fn claude_creds_file(&self) -> PathBuf {
        self.claude_config_dir.join(".credentials.json")
    }

    fn codex_auth_file(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn root_dir(&self) -> PathBuf {
        self.data_dir.join("agent-accounts")
    }
}

// ── slot storage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlotProfile {
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    auth_kind: AgentAuthKind,
}

/// One saved login (`{slotId}.json`), same field surface as comet's slot files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    id: String,
    harness: HarnessId,
    /// The provider-side identity the slot is keyed by (account uuid/email).
    account_key: String,
    profile: SlotProfile,
    /// Claude: the `.credentials.json`/Keychain payload. Codex: `auth.json`.
    credentials: serde_json::Value,
    /// Claude only: `{oauthAccount, userID}` merged into `~/.claude.json` on swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_config: Option<serde_json::Value>,
    saved_at: i64,
    /// First time this account was saved — the STABLE sort key, so switching the
    /// active account (which re-snapshots and bumps `saved_at`) never reorders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

/// A live detection result (before it's persisted into a slot).
#[derive(Debug, Clone)]
struct Detected {
    account_key: String,
    profile: SlotProfile,
    /// `None` ⇒ we know a login exists but couldn't read the secret.
    credentials: Option<serde_json::Value>,
    claude_config: Option<serde_json::Value>,
}

// ── login flows ─────────────────────────────────────────────────────────────

enum LoginFlow {
    Claude {
        verifier: String,
        started_at: Instant,
    },
    Codex {
        /// The `codex login` child; monitored (try_wait) + killable from cancel.
        child: Arc<Mutex<Option<tokio::process::Child>>>,
        /// Throwaway `CODEX_HOME` — the live `~/.codex` is never touched.
        home: PathBuf,
        started_at: Instant,
        output: Arc<Mutex<String>>,
        /// `Some(code)` once the child exited (`None` code = killed by signal).
        exit: Arc<Mutex<Option<Option<i32>>>>,
    },
}

impl LoginFlow {
    fn started_at(&self) -> Instant {
        match self {
            LoginFlow::Claude { started_at, .. } | LoginFlow::Codex { started_at, .. } => {
                *started_at
            }
        }
    }
}

// ── service ─────────────────────────────────────────────────────────────────

/// Cached usage probe result: the windows (or a remembered miss) + fetch time.
type CachedUsage = (Option<Vec<AgentUsageWindow>>, Instant);

struct Inner {
    config: AgentAccountsConfig,
    http: reqwest::Client,
    flows: Mutex<HashMap<String, LoginFlow>>,
    /// `"{harness}:{accountKey}"` → cached usage windows.
    usage_cache: Mutex<HashMap<String, CachedUsage>>,
    /// Slots with a token refresh in flight — a second refresh of the same
    /// (commonly single-use) refresh token would revoke the family.
    inflight_refreshes: Mutex<std::collections::HashSet<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct AgentAccounts {
    inner: Arc<Inner>,
}

impl AgentAccounts {
    pub fn new(config: AgentAccountsConfig) -> Self {
        // Startup sweep: a previous process that crashed mid-login leaves
        // `.login-<uuid>` throwaway CODEX_HOME dirs — each may hold live OAuth
        // tokens — with no owner to clean them. Reclaim them at boot.
        let root = config.root_dir();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".login-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                flows: Mutex::new(HashMap::new()),
                usage_cache: Mutex::new(HashMap::new()),
                inflight_refreshes: Mutex::new(std::collections::HashSet::new()),
            }),
        }
    }

    // ── list ────────────────────────────────────────────────────────────────

    /// Detect both CLIs, auto-snapshot the live logins, and assemble the view.
    pub async fn list(&self, force_usage: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        if force_usage {
            lock(&self.inner.usage_cache).clear();
        }
        let mut warnings: Vec<AgentAccountWarning> = Vec::new();
        let mut active_keys: HashMap<HarnessId, String> = HashMap::new();
        let mut unreadable: HashMap<HarnessId, Detected> = HashMap::new();

        let (claude, claude_warning) = self.detect_claude().await;
        if let Some(message) = claude_warning {
            warnings.push(AgentAccountWarning {
                harness: HarnessId::ClaudeCode,
                message,
            });
        }
        if let Some(detected) = claude {
            active_keys.insert(HarnessId::ClaudeCode, detected.account_key.clone());
            if detected.credentials.is_some() {
                self.snapshot_detected(HarnessId::ClaudeCode, &detected)?;
            } else {
                unreadable.insert(HarnessId::ClaudeCode, detected);
            }
        }
        if let Some(detected) = self.detect_codex() {
            active_keys.insert(HarnessId::Codex, detected.account_key.clone());
            self.snapshot_detected(HarnessId::Codex, &detected)?;
        }

        // Stable presentation order: provider, then slot creation order (never
        // active-first — switching must not reshuffle the cards).
        let mut accounts: Vec<AgentAccount> = Vec::new();
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex] {
            let active_key = active_keys.get(&harness).cloned();
            let slots = self.read_slots(harness);
            for slot in &slots {
                let active = active_key.as_deref() == Some(slot.account_key.as_str());
                let usage = self.usage_for(harness, slot, active, force_usage).await;
                accounts.push(AgentAccount {
                    id: slot.id.clone(),
                    harness,
                    email: Some(slot.profile.email.clone()),
                    plan_label: slot.profile.plan.clone(),
                    active,
                    usage_windows: usage.unwrap_or_default(),
                    display_name: slot.profile.display_name.clone(),
                    organization: slot.profile.organization.clone(),
                    auth_kind: Some(slot.profile.auth_kind),
                    switchable: true,
                    saved_at: Some(slot.saved_at),
                });
            }
            // A live login whose credentials we couldn't read has no slot — still
            // show it (active, but not re-activatable until the Keychain relents).
            if let Some(u) = unreadable.get(&harness)
                && !slots.iter().any(|s| s.account_key == u.account_key)
            {
                accounts.push(AgentAccount {
                    id: slot_id_for(harness, &u.account_key),
                    harness,
                    email: Some(u.profile.email.clone()),
                    plan_label: u.profile.plan.clone(),
                    active: true,
                    usage_windows: Vec::new(),
                    display_name: u.profile.display_name.clone(),
                    organization: u.profile.organization.clone(),
                    auth_kind: Some(u.profile.auth_kind),
                    switchable: false,
                    saved_at: None,
                });
            }
        }
        Ok(AgentAccountsSnapshot { accounts, warnings })
    }

    // ── swap ────────────────────────────────────────────────────────────────

    /// Swap the CLI's live login to a saved slot. Detection runs first, so the
    /// CURRENT login is snapshotted into its slot before being overwritten (the
    /// claude-swap trick — a swap never strands the session it replaces).
    pub async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        self.list(false).await?;
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)
            .ok_or_else(|| {
                EngineError::Other(
                    "That saved login no longer exists — refresh and try again.".into(),
                )
            })?;
        match harness {
            HarnessId::ClaudeCode => self.activate_claude(&slot).await?,
            HarnessId::Codex => self.activate_codex(&slot)?,
            other => {
                return Err(EngineError::Other(format!(
                    "agent accounts are not supported for {other:?}"
                )));
            }
        }
        self.list(false).await
    }

    async fn activate_claude(&self, slot: &Slot) -> Result<(), EngineError> {
        self.write_claude_credentials(&slot.credentials).await?;
        // Merge the identity back into ~/.claude.json — everything else (caches,
        // project history, onboarding flags) is left untouched, which is all
        // Claude Code needs to treat this as a fresh login.
        //
        // GUARD the merge: a parse failure on an EXISTING file means "don't touch
        // it", not "start fresh" — writing only our identity fields would destroy
        // the user's entire Claude config. Only a missing file may start from {}.
        let file = &self.inner.config.claude_config_file;
        let cfg = read_json(file);
        if cfg.is_none() && file.exists() {
            return Err(EngineError::Other(
                "~/.claude.json exists but could not be parsed — not switching to avoid wiping \
                 it. Fix or remove the file and try again."
                    .into(),
            ));
        }
        let mut merged = cfg.unwrap_or_else(|| serde_json::json!({}));
        let map = merged.as_object_mut().ok_or_else(|| {
            EngineError::Other("~/.claude.json is not a JSON object — not switching.".into())
        })?;
        let (oauth_account, user_id) = match &slot.claude_config {
            Some(cc) => (cc.get("oauthAccount").cloned(), cc.get("userID").cloned()),
            None => (None, None),
        };
        map.insert(
            "oauthAccount".into(),
            oauth_account.unwrap_or_else(|| {
                serde_json::json!({
                    "accountUuid": slot.account_key,
                    "emailAddress": slot.profile.email,
                    "organizationName": slot.profile.organization,
                    "displayName": slot.profile.display_name,
                })
            }),
        );
        match user_id.filter(|v| v.is_string()) {
            Some(user_id) => {
                map.insert("userID".into(), user_id);
            }
            None => {
                map.remove("userID");
            }
        }
        // Atomic: Claude Code rewrites this file frequently — a torn write from
        // our side must never be readable as "empty config".
        write_file_atomic(file, merged.to_string().as_bytes(), false)
    }

    fn activate_codex(&self, slot: &Slot) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.inner.config.codex_home)?;
        let json = serde_json::to_string_pretty(&slot.credentials)
            .map_err(|e| EngineError::Other(format!("serialize codex auth: {e}")))?;
        write_file_atomic(&self.inner.config.codex_auth_file(), json.as_bytes(), true)
    }

    // ── forget ──────────────────────────────────────────────────────────────

    pub async fn forget(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        // Reject anything that isn't a slot id (16 lowercase hex) BEFORE touching
        // the filesystem: `account_id` is a raw RPC string that becomes a path,
        // so a crafted id (`../../…`) must never reach `remove_file`.
        if account_id.len() != 16
            || !account_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(EngineError::Other("Unknown account.".into()));
        }
        let snapshot = self.list(false).await?;
        let active = snapshot
            .accounts
            .iter()
            .any(|a| a.harness == harness && a.id == account_id && a.active);
        if active {
            return Err(EngineError::Other(
                "That's the live login — switch to another account first (it would just be \
                 re-detected)."
                    .into(),
            ));
        }
        let file = self.slots_dir(harness)?.join(format!("{account_id}.json"));
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        self.list(false).await
    }

    // ── add-account OAuth flows ─────────────────────────────────────────────

    pub async fn start_login(&self, harness: HarnessId) -> Result<AgentLoginStart, EngineError> {
        self.sweep_flows();
        match harness {
            HarnessId::ClaudeCode => Ok(self.start_claude_login()),
            HarnessId::Codex => self.start_codex_login().await,
            other => Err(EngineError::Other(format!(
                "agent logins are not supported for {other:?}"
            ))),
        }
    }

    fn start_claude_login(&self) -> AgentLoginStart {
        let login_id = new_id();
        // PKCE: 32 random bytes (two v4 uuids) as the verifier, S256 challenge.
        let raw: Vec<u8> = uuid::Uuid::new_v4()
            .as_bytes()
            .iter()
            .chain(uuid::Uuid::new_v4().as_bytes())
            .copied()
            .collect();
        let verifier = BASE64_URL.encode(&raw);
        let challenge = BASE64_URL.encode(Sha256::digest(verifier.as_bytes()));
        let url = format!(
            "https://claude.ai/oauth/authorize?code=true&client_id={CLAUDE_CLIENT_ID}\
             &response_type=code&redirect_uri={}&scope={}&code_challenge={challenge}\
             &code_challenge_method=S256&state={verifier}",
            urlencode(CLAUDE_REDIRECT),
            urlencode(CLAUDE_SCOPES),
        );
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Claude {
                verifier,
                started_at: Instant::now(),
            },
        );
        AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::PasteCode,
        }
    }

    async fn start_codex_login(&self) -> Result<AgentLoginStart, EngineError> {
        // At most ONE codex login flow at a time: `codex login` binds a fixed
        // loopback OAuth port, so a lingering earlier flow makes every retry exit
        // on EADDRINUSE. Starting a new flow supersedes — and reaps — any pending.
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| matches!(f, LoginFlow::Codex { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }

        let login_id = new_id();
        // A throwaway CODEX_HOME isolates the new login completely — the live
        // ~/.codex session is never touched until the user explicitly switches.
        let home = self
            .inner
            .config
            .root_dir()
            .join(format!(".login-{login_id}"));
        std::fs::create_dir_all(&home)?;
        let mut child = match tokio::process::Command::new("codex")
            .arg("login")
            .env("CODEX_HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(EngineError::Other(
                    if err.kind() == std::io::ErrorKind::NotFound {
                        "The `codex` CLI was not found on this device — install it first.".into()
                    } else {
                        format!("Could not start codex login: {err}")
                    },
                ));
            }
        };

        // codex prints the authorize URL (to stderr as of 0.142 — scan both
        // streams) and usually opens the browser itself; grab it so the app can
        // open it too.
        let output = Arc::new(Mutex::new(String::new()));
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let sink = output.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    lock(&sink).push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }

        let child = Arc::new(Mutex::new(Some(child)));
        let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
        {
            // Monitor: poll try_wait so the child is reaped without owning it —
            // the cancel path needs concurrent kill access.
            let child = child.clone();
            let exit = exit.clone();
            tokio::spawn(async move {
                loop {
                    {
                        let mut slot = lock(&child);
                        match slot.as_mut().map(|c| c.try_wait()) {
                            None => break,
                            Some(Ok(Some(status))) => {
                                *lock(&exit) = Some(status.code());
                                *slot = None;
                                break;
                            }
                            Some(Ok(None)) => {}
                            Some(Err(_)) => {
                                *lock(&exit) = Some(None);
                                *slot = None;
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }

        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Codex {
                child,
                home,
                started_at: Instant::now(),
                output: output.clone(),
                exit: exit.clone(),
            },
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let url = loop {
            if let Some(url) = scan_openai_url(&lock(&output)) {
                break url;
            }
            if lock(&exit).is_some() || Instant::now() > deadline {
                break String::new();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok(AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::Browser,
        })
    }

    /// Exchange the pasted `code#state` for tokens and save the account as a slot
    /// (the live login is untouched — switching is an explicit, separate act).
    pub async fn complete_login(
        &self,
        login_id: &str,
        code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        let verifier = match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::Claude { verifier, .. }) => verifier.clone(),
            _ => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
        };
        let (auth_code, state) = match code.trim().split_once('#') {
            Some((c, s)) => (c.to_string(), s.to_string()),
            None => (code.trim().to_string(), verifier.clone()),
        };
        if auth_code.is_empty() {
            return Err(EngineError::Other(
                "That code looks empty — paste the whole code.".into(),
            ));
        }
        let token = self
            .inner
            .http
            .post(CLAUDE_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "code": auth_code,
                "state": state,
                "client_id": CLAUDE_CLIENT_ID,
                "redirect_uri": CLAUDE_REDIRECT,
                "code_verifier": verifier,
            }))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange failed: {e}")))?;
        if !token.status().is_success() {
            let status = token.status();
            let body = token.text().await.unwrap_or_default();
            let excerpt: String = body.chars().take(200).collect();
            return Err(EngineError::Other(format!(
                "Anthropic rejected the code ({status}): {excerpt}"
            )));
        }
        let token: serde_json::Value = token
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange returned junk: {e}")))?;

        let access_token = str_field(&token, "access_token");
        let refresh_token = str_field(&token, "refresh_token");
        let expires_in = token
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token) else {
            return Err(EngineError::Other(
                "Anthropic returned no usable tokens — try signing in again.".into(),
            ));
        };

        // Best-effort profile fetch — fills in the plan/org the way Claude Code does.
        let profile: Option<serde_json::Value> = match self
            .inner
            .http
            .get(CLAUDE_PROFILE_URL)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => res.json().await.ok(),
            _ => None,
        };
        let empty = serde_json::json!({});
        let p_account = profile
            .as_ref()
            .and_then(|p| p.get("account"))
            .unwrap_or(&empty);
        let p_org = profile
            .as_ref()
            .and_then(|p| p.get("organization"))
            .unwrap_or(&empty);
        let t_account = token.get("account").unwrap_or(&empty);
        let t_org = token.get("organization").unwrap_or(&empty);

        let email = str_field(p_account, "email_address")
            .or_else(|| str_field(t_account, "email_address"))
            .ok_or_else(|| {
                EngineError::Other("Could not identify the signed-in account.".into())
            })?;
        let account_uuid = str_field(p_account, "uuid")
            .or_else(|| str_field(t_account, "uuid"))
            .unwrap_or_else(|| email.clone());
        let org_name = str_field(p_org, "name").or_else(|| str_field(t_org, "name"));
        let org_type = str_field(p_org, "organization_type");
        let rate_tier = str_field(p_org, "rate_limit_tier");
        let display_name =
            str_field(p_account, "display_name").or_else(|| str_field(p_account, "full_name"));
        let subscription_type = match org_type.as_deref() {
            Some("claude_max") => Some("max"),
            Some("claude_pro") => Some("pro"),
            Some("claude_team") => Some("team"),
            Some("claude_enterprise") => Some("enterprise"),
            _ => None,
        };

        let scopes: Vec<String> = str_field(&token, "scope")
            .unwrap_or_else(|| CLAUDE_SCOPES.to_string())
            .split(' ')
            .map(str::to_string)
            .collect();
        let mut oauth = serde_json::json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": now_ms() + expires_in * 1000,
            "scopes": scopes,
        });
        if let (Some(sub), Some(map)) = (subscription_type, oauth.as_object_mut()) {
            map.insert("subscriptionType".into(), serde_json::json!(sub));
        }
        let mut oauth_account = serde_json::json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": str_field(p_org, "uuid").or_else(|| str_field(t_org, "uuid")),
            "organizationName": org_name,
            "displayName": display_name,
        });
        if let Some(map) = oauth_account.as_object_mut() {
            if let Some(t) = &org_type {
                map.insert("organizationType".into(), serde_json::json!(t));
            }
            if let Some(t) = &rate_tier {
                map.insert("organizationRateLimitTier".into(), serde_json::json!(t));
            }
        }

        self.write_slot(&Slot {
            id: slot_id_for(HarnessId::ClaudeCode, &account_uuid),
            harness: HarnessId::ClaudeCode,
            account_key: account_uuid.clone(),
            profile: SlotProfile {
                email,
                display_name,
                organization: org_name,
                plan: claude_plan(org_type.as_deref(), rate_tier.as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: serde_json::json!({ "claudeAiOauth": oauth }),
            claude_config: Some(serde_json::json!({ "oauthAccount": oauth_account })),
            saved_at: now_ms(),
            created_at: None,
        })?;
        lock(&self.inner.flows).remove(login_id);
        self.list(false).await
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<AgentLoginPoll, EngineError> {
        self.sweep_flows();
        let (home, exit, output) = match lock(&self.inner.flows).get(login_id) {
            None => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
            Some(LoginFlow::Claude { .. }) => {
                return Ok(AgentLoginPoll {
                    status: AgentLoginStatus::Pending,
                    message: None,
                });
            }
            Some(LoginFlow::Codex {
                home, exit, output, ..
            }) => (home.clone(), exit.clone(), output.clone()),
        };
        if let Some(detected) = read_json(&home.join("auth.json")).and_then(parse_codex_auth) {
            self.snapshot_detected(HarnessId::Codex, &detected)?;
            self.cancel_login(login_id);
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Done,
                message: None,
            });
        }
        let exited = *lock(&exit);
        if let Some(code) = exited {
            self.cancel_login(login_id);
            let message = if code == Some(0) {
                "codex login finished without credentials.".to_string()
            } else {
                lock(&output)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("sign-in failed")
                    .to_string()
            };
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Error,
                message: Some(message),
            });
        }
        Ok(AgentLoginPoll {
            status: AgentLoginStatus::Pending,
            message: None,
        })
    }

    /// Drop a flow: kill a pending `codex login` child (it holds the fixed
    /// loopback OAuth port) and reclaim its throwaway home dir. Idempotent.
    pub fn cancel_login(&self, login_id: &str) {
        let flow = lock(&self.inner.flows).remove(login_id);
        if let Some(LoginFlow::Codex { child, home, .. }) = flow {
            if let Some(c) = lock(&child).as_mut() {
                let _ = c.start_kill();
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// Engine shutdown: kill any in-flight login child so an orphan `codex login`
    /// can't survive the restart and brick the next attempt.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.inner.flows).keys().cloned().collect();
        for id in ids {
            self.cancel_login(&id);
        }
    }

    /// Lazy TTL sweep (comet uses a background fiber; native reaps on the next
    /// accounts call — same bound, no standing task).
    fn sweep_flows(&self) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| f.started_at().elapsed() > FLOW_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    // ── detection ───────────────────────────────────────────────────────────

    async fn detect_claude(&self) -> (Option<Detected>, Option<String>) {
        let cfg = read_json(&self.inner.config.claude_config_file);
        let Some(oauth) = cfg.as_ref().and_then(|c| c.get("oauthAccount")).cloned() else {
            return (None, None);
        };
        let Some(email) = str_field(&oauth, "emailAddress") else {
            return (None, None);
        };
        let (credentials, warning) = self.read_claude_credentials().await;
        let user_id = cfg.as_ref().and_then(|c| c.get("userID")).cloned();
        let mut claude_config = serde_json::json!({ "oauthAccount": oauth });
        if let (Some(uid), Some(map)) = (user_id, claude_config.as_object_mut())
            && uid.is_string()
        {
            map.insert("userID".into(), uid);
        }
        (
            Some(Detected {
                account_key: str_field(&oauth, "accountUuid").unwrap_or_else(|| email.clone()),
                profile: SlotProfile {
                    email,
                    display_name: str_field(&oauth, "displayName"),
                    organization: str_field(&oauth, "organizationName"),
                    plan: claude_plan(
                        str_field(&oauth, "organizationType").as_deref(),
                        str_field(&oauth, "organizationRateLimitTier").as_deref(),
                    ),
                    auth_kind: AgentAuthKind::Oauth,
                },
                credentials,
                claude_config: Some(claude_config),
            }),
            warning,
        )
    }

    fn detect_codex(&self) -> Option<Detected> {
        read_json(&self.inner.config.codex_auth_file()).and_then(parse_codex_auth)
    }

    /// Persist a detected login into its slot (refreshing stored tokens).
    fn snapshot_detected(&self, harness: HarnessId, d: &Detected) -> Result<(), EngineError> {
        let Some(credentials) = &d.credentials else {
            return Ok(());
        };
        self.write_slot(&Slot {
            id: slot_id_for(harness, &d.account_key),
            harness,
            account_key: d.account_key.clone(),
            profile: d.profile.clone(),
            credentials: credentials.clone(),
            claude_config: d.claude_config.clone(),
            saved_at: now_ms(),
            created_at: None,
        })
    }

    // ── Claude credential store (Keychain on macOS, file elsewhere) ─────────

    /// Read the live Claude credentials. `None` payload + warning ⇒ we know a
    /// login exists but couldn't read the secret (Keychain denied us).
    async fn read_claude_credentials(&self) -> (Option<serde_json::Value>, Option<String>) {
        if let Some(creds) = read_json(&self.inner.config.claude_creds_file()) {
            return (Some(creds), None);
        }
        #[cfg(target_os = "macos")]
        {
            return keychain::read_credentials().await;
        }
        #[cfg(not(target_os = "macos"))]
        (None, None)
    }

    async fn write_claude_credentials(
        &self,
        credentials: &serde_json::Value,
    ) -> Result<(), EngineError> {
        let json = credentials.to_string();
        #[cfg(target_os = "macos")]
        {
            // claude-swap's primitive: update the Keychain item in place — but only
            // when no credentials FILE exists (the file wins when present).
            if !self.inner.config.claude_creds_file().exists() {
                return keychain::write_credentials(&json).await;
            }
        }
        std::fs::create_dir_all(&self.inner.config.claude_config_dir)?;
        // Atomic + owner-only from birth — live tokens.
        write_file_atomic(
            &self.inner.config.claude_creds_file(),
            json.as_bytes(),
            true,
        )
    }

    // ── slot files ──────────────────────────────────────────────────────────

    fn slots_dir(&self, harness: HarnessId) -> Result<PathBuf, EngineError> {
        let dir = self.inner.config.root_dir().join(harness_slug(harness));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn read_slots(&self, harness: HarnessId) -> Vec<Slot> {
        let Ok(dir) = self.slots_dir(harness) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One malformed slot file must skip THAT slot, not brick the page.
            if let Some(slot) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Slot>(&raw).ok())
            {
                slots.push(slot);
            }
        }
        // Creation order — stable across switches (saved_at churns on every
        // auto-snapshot; created_at never does).
        slots.sort_by_key(|s| s.created_at.unwrap_or(s.saved_at));
        slots
    }

    fn write_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self
            .slots_dir(slot.harness)?
            .join(format!("{}.json", slot.id));
        let existing: Option<Slot> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut full = slot.clone();
        full.created_at = existing
            .and_then(|e| e.created_at.or(Some(e.saved_at)))
            .or(slot.created_at)
            .or(Some(slot.saved_at));
        let json = serde_json::to_string_pretty(&full)
            .map_err(|e| EngineError::Other(format!("serialize slot: {e}")))?;
        // Atomic + 0600 from birth: tokens must never be world-readable, and a
        // crash mid-write must never leave torn JSON.
        write_file_atomic(&file, json.as_bytes(), true)
    }

    // ── remaining usage ─────────────────────────────────────────────────────

    async fn usage_for(
        &self,
        harness: HarnessId,
        slot: &Slot,
        is_active: bool,
        force: bool,
    ) -> Option<Vec<AgentUsageWindow>> {
        let key = format!("{}:{}", harness_slug(harness), slot.account_key);
        if let Some((usage, at)) = lock(&self.inner.usage_cache).get(&key)
            && at.elapsed() < USAGE_TTL
        {
            return usage.clone();
        }
        if !force {
            // Non-forced lists never hit the network (see module docs).
            return None;
        }
        let usage = match harness {
            HarnessId::ClaudeCode => self.claude_usage(slot, is_active).await,
            HarnessId::Codex => self.codex_usage(slot).await,
            _ => None,
        };
        lock(&self.inner.usage_cache).insert(key, (usage.clone(), Instant::now()));
        usage
    }

    async fn claude_usage(&self, slot: &Slot, is_active: bool) -> Option<Vec<AgentUsageWindow>> {
        let oauth = slot.credentials.get("claudeAiOauth")?;
        let mut access_token = str_field(oauth, "accessToken")?;
        let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());
        if let Some(expires_at) = expires_at
            && expires_at < now_ms() + 30_000
        {
            if is_active {
                // The CLI owns this token pair — rotating its refresh token out
                // from under a running Claude Code could force a re-login.
                return None;
            }
            access_token = self.refresh_claude_slot(slot).await?;
        }
        let body: serde_json::Value = self
            .inner
            .http
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let mut windows = Vec::new();
        for (key, label) in [("five_hour", "Session"), ("seven_day", "Week")] {
            if let Some(w) = body.get(key)
                && let Some(utilization) = w.get("utilization").and_then(|v| v.as_f64())
            {
                windows.push(AgentUsageWindow {
                    label: label.to_string(),
                    used_fraction: (utilization / 100.0) as f32,
                    resets_at: parse_when(w.get("resets_at")),
                });
            }
        }
        (!windows.is_empty()).then_some(windows)
    }

    async fn codex_usage(&self, slot: &Slot) -> Option<Vec<AgentUsageWindow>> {
        let tokens = slot.credentials.get("tokens")?;
        // api-key mode has no ChatGPT rate windows.
        let access_token = str_field(tokens, "access_token")?;
        let body: serde_json::Value = self
            .inner
            .http
            .get(CODEX_USAGE_URL)
            .bearer_auth(&access_token)
            .header(
                "chatgpt-account-id",
                str_field(tokens, "account_id").unwrap_or_default(),
            )
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let rl = body.get("rate_limit")?;
        let mut windows = Vec::new();
        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = rl.get(key)
                && let Some(used) = w.get("used_percent").and_then(|v| v.as_f64())
            {
                let span = w
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                windows.push(AgentUsageWindow {
                    label: if span > 86_400 { "Week" } else { "Session" }.to_string(),
                    used_fraction: (used / 100.0) as f32,
                    resets_at: parse_when(w.get("reset_at")),
                });
            }
        }
        (!windows.is_empty()).then_some(windows)
    }

    /// Refresh a saved Claude slot's expired access token so its usage stays
    /// queryable. NEVER called for the active login. Single-flight per slot:
    /// OAuth refresh tokens are commonly single-use, and a concurrent second
    /// POST of the same one would revoke the family and brick the slot.
    async fn refresh_claude_slot(&self, slot: &Slot) -> Option<String> {
        if !lock(&self.inner.inflight_refreshes).insert(slot.id.clone()) {
            return None;
        }
        let result = self.refresh_claude_slot_once(slot).await;
        lock(&self.inner.inflight_refreshes).remove(&slot.id);
        result
    }

    async fn refresh_claude_slot_once(&self, slot: &Slot) -> Option<String> {
        let oauth = slot.credentials.get("claudeAiOauth")?.clone();
        let refresh_token = str_field(&oauth, "refreshToken")?;
        let body: serde_json::Value = self
            .inner
            .http
            .post(CLAUDE_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLAUDE_CLIENT_ID,
            }))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let access_token = str_field(&body, "access_token")?;
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let mut updated = oauth;
        if let Some(map) = updated.as_object_mut() {
            map.insert("accessToken".into(), serde_json::json!(access_token));
            map.insert(
                "refreshToken".into(),
                serde_json::json!(str_field(&body, "refresh_token").unwrap_or(refresh_token)),
            );
            map.insert(
                "expiresAt".into(),
                serde_json::json!(now_ms() + expires_in * 1000),
            );
        }
        let mut refreshed = slot.clone();
        refreshed.credentials = serde_json::json!({ "claudeAiOauth": updated });
        refreshed.saved_at = now_ms();
        if let Err(err) = self.write_slot(&refreshed) {
            tracing::warn!(slot = %slot.id, error = %err, "refreshed slot write failed");
        }
        Some(access_token)
    }
}

// ── macOS Keychain (documented here; compiled only on macOS) ────────────────
//
// Claude Code stores its credentials in the login Keychain under the service
// `Claude Code-credentials`, account = the current username. Reads use
// `security find-generic-password` — two-step (existence probe needs no
// authorization, then `-w` for the secret) so a user denial is distinguishable
// from "not logged in". Writes use `add-generic-password -U` (update in place).
// Every call is bounded at 15s: an unanswered Keychain consent dialog blocks
// `security` INDEFINITELY, and this runs on every list.
#[cfg(target_os = "macos")]
mod keychain {
    use super::*;

    const EXEC_TIMEOUT: Duration = Duration::from_secs(15);

    async fn exec(args: &[&str]) -> (bool, String, String) {
        let run = tokio::process::Command::new("security")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(EXEC_TIMEOUT, run).await {
            Ok(Ok(out)) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ),
            _ => (false, String::new(), "security timed out".into()),
        }
    }

    fn account() -> String {
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    }

    pub(super) async fn read_credentials() -> (Option<serde_json::Value>, Option<String>) {
        let (probe_ok, ..) = exec(&["find-generic-password", "-s", KEYCHAIN_SERVICE]).await;
        if !probe_ok {
            return (None, None);
        }
        let (ok, stdout, _) = exec(&[
            "find-generic-password",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ])
        .await;
        if !ok {
            return (
                None,
                Some(
                    "A Claude Code login exists, but macOS Keychain denied access to it — \
                     approve the prompt (choose “Always Allow”) and refresh to enable switching."
                        .into(),
                ),
            );
        }
        match serde_json::from_str(stdout.trim()) {
            Ok(creds) => (Some(creds), None),
            Err(_) => (
                None,
                Some("The Claude Code Keychain entry could not be parsed.".into()),
            ),
        }
    }

    pub(super) async fn write_credentials(json: &str) -> Result<(), EngineError> {
        let (ok, _, stderr) = exec(&[
            "add-generic-password",
            "-U",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            json,
        ])
        .await;
        if ok {
            Ok(())
        } else {
            Err(EngineError::Other(format!(
                "Keychain write failed: {}",
                if stderr.trim().is_empty() {
                    "unknown error"
                } else {
                    stderr.trim()
                }
            )))
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn harness_slug(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

fn read_json(file: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT payload without verifying — we only mine identity claims from a
/// token the user's own CLI already trusts.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = BASE64_URL
        .decode(payload)
        .or_else(|_| BASE64.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn slot_id_for(harness: HarnessId, account_key: &str) -> String {
    let digest = Sha256::digest(format!("{}:{account_key}", harness_slug(harness)).as_bytes());
    crate::repos::hex(&digest)[..16].to_string()
}

/// Pretty plan label from Claude's org type + rate-limit tier ("Max 20×").
fn claude_plan(org_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    let base = match org_type {
        Some("claude_max") => "Max",
        Some("claude_pro") => "Pro",
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        _ => return None,
    };
    // "…_20x" style tiers carry a multiplier suffix.
    let mult = tier.and_then(|t| {
        let stem = t.strip_suffix('x')?;
        let digits: String = stem
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let preceded = stem.len() > digits.len()
            && stem.as_bytes().get(stem.len() - digits.len() - 1) == Some(&b'_');
        (!digits.is_empty() && preceded).then_some(digits)
    });
    Some(match mult {
        Some(mult) => format!("{base} {mult}×"),
        None => base.to_string(),
    })
}

fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Parse a codex `auth.json` (the live one or a fresh login's).
fn parse_codex_auth(auth: serde_json::Value) -> Option<Detected> {
    if let Some(id_token) = auth
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
    {
        let claims = jwt_claims(id_token).unwrap_or_else(|| serde_json::json!({}));
        let oa = claims
            .get("https://api.openai.com/auth")
            .cloned()
            .unwrap_or_default();
        let email = str_field(&claims, "email")?;
        return Some(Detected {
            account_key: str_field(&oa, "chatgpt_account_id").unwrap_or_else(|| email.clone()),
            profile: SlotProfile {
                email,
                display_name: str_field(&claims, "name"),
                organization: None,
                plan: codex_plan(str_field(&oa, "chatgpt_plan_type").as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: Some(auth),
            claude_config: None,
        });
    }
    let api_key = str_field(&auth, "OPENAI_API_KEY")?;
    let digest = Sha256::digest(api_key.as_bytes());
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(Detected {
        account_key: format!("api-key:{}", &crate::repos::hex(&digest)[..12]),
        profile: SlotProfile {
            email: format!("API key ·…{tail}"),
            display_name: None,
            organization: None,
            plan: Some("API key".into()),
            auth_kind: AgentAuthKind::ApiKey,
        },
        credentials: Some(auth),
        claude_config: None,
    })
}

/// ISO string (Claude) or unix seconds (Codex) → timestamp.
fn parse_when(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    match value? {
        serde_json::Value::Number(n) => DateTime::<Utc>::from_timestamp(n.as_i64()?, 0),
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}

fn scan_openai_url(output: &str) -> Option<String> {
    let start = output.find("https://auth.openai.com/")?;
    let rest = &output[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Minimal percent-encoding for OAuth query params (matches `encodeURIComponent`
/// for the constant inputs used here).
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Atomic write via a same-dir temp file + rename; `secret` = 0600 from birth.
fn write_file_atomic(file: &Path, bytes: &[u8], secret: bool) -> Result<(), EngineError> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut handle = options.open(&tmp)?;
        handle.write_all(bytes)?;
    }
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_labels() {
        assert_eq!(
            claude_plan(Some("claude_max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20×")
        );
        assert_eq!(
            claude_plan(Some("claude_pro"), None).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            claude_plan(Some("claude_team"), Some("weird")).as_deref(),
            Some("Team")
        );
        assert_eq!(claude_plan(Some("free"), None), None);
        assert_eq!(codex_plan(Some("plus")).as_deref(), Some("ChatGPT Plus"));
        assert_eq!(codex_plan(None), None);
    }

    #[test]
    fn openai_url_scan() {
        assert_eq!(
            scan_openai_url("open https://auth.openai.com/authorize?x=1 in your browser\n")
                .as_deref(),
            Some("https://auth.openai.com/authorize?x=1")
        );
        assert_eq!(scan_openai_url("nothing here"), None);
    }

    #[test]
    fn urlencode_matches_encode_uri_component() {
        assert_eq!(
            urlencode("org:create_api_key user:profile"),
            "org%3Acreate_api_key%20user%3Aprofile"
        );
        assert_eq!(urlencode("https://a/b"), "https%3A%2F%2Fa%2Fb");
    }
}
