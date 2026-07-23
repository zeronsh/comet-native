//! Auth — the engine owns the WorkOS session for its device (feature-inventory §3.7,
//! ARCHITECTURE §5). Port of comet's `apps/backend/src/auth.ts`.
//!
//! The engine is a public client: it builds the AuthKit authorize URL itself but
//! delegates the secret-bearing **code exchange** and **refresh** to the edge Worker
//! (`/auth/exchange`, `/auth/refresh` — the WorkOS API key lives only there).
//!
//! Two modes:
//! - **Dev** (no WorkOS client id configured, or the edge reports `auth: "dev"`): always
//!   signed in; the bearer IS the configured user id (current M2/M3 behavior).
//! - **WorkOS**: authorization-code flow. Headed devices use a loopback callback server
//!   on an ephemeral port; headless devices use the paste-code flow (the redirect is the
//!   edge's hosted `/auth/cli/callback` page, which shows `state.code` to paste back via
//!   stdin or the `CompleteSignIn` RPC). The refresh token is persisted 0600 in the data
//!   dir; access tokens are cached with dual-clock expiry (monotonic AND wall, whichever
//!   aged more — see [`AccessEntry`]) and refreshed on demand plus by a background loop,
//!   so the device-room relay and room clients always dial with a live `?token=`, even
//!   on the first redial after a laptop wakes from sleep. Org onboarding: an org-less session is `NeedsOrganization`; `SelectOrg`
//!   runs an org-scoped refresh and the state follows the returned token's `org_id`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::EngineError;

const SIGN_IN_TTL: Duration = Duration::from_secs(15 * 60);
/// Refresh when the cached token has less than this much life left.
const TOKEN_SLACK: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Wire types (feature-inventory §2 AuthRpc)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMembership {
    pub id: String,
    pub organization_id: String,
    pub name: String,
}

/// AuthStatus stream payload (`SignedOut | NeedsOrganization{user} |
/// SignedIn{user, orgId?}`). Serializes as the canonical [`comet_proto::AuthState`]
/// wire shape (`{"state": "signedIn", …}`) so every client parses one form.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    NeedsOrganization {
        user: AuthUser,
    },
    SignedIn {
        user: AuthUser,
        org_id: Option<String>,
    },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, AuthState::SignedIn { .. })
    }

    pub fn org_id(&self) -> Option<&str> {
        match self {
            AuthState::SignedIn { org_id, .. } => org_id.as_deref(),
            _ => None,
        }
    }

    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    /// The proto wire twin — the one shape the engine emits over AuthStatus.
    pub fn to_proto(&self) -> comet_proto::AuthState {
        let profile = |user: &AuthUser| comet_proto::UserProfile {
            id: user.id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        };
        match self {
            AuthState::SignedOut => comet_proto::AuthState::SignedOut,
            AuthState::NeedsOrganization { user } => comet_proto::AuthState::NeedsOrganization {
                user: profile(user),
            },
            AuthState::SignedIn { user, org_id } => comet_proto::AuthState::SignedIn {
                user: profile(user),
                org_id: org_id.clone(),
            },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

// ---------------------------------------------------------------------------
// Config + construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Edge base URL (`/auth/*` routes).
    pub edge_url: String,
    /// Data dir for the persisted session (`session.json`, 0600).
    pub data_dir: PathBuf,
    /// WorkOS client id; `None` = dev mode.
    pub workos_client_id: Option<String>,
    /// WorkOS API base (authorize URL host).
    pub workos_api_base: String,
    /// Dev-mode bearer/user id (mirrors the old `COMET_EDGE_TOKEN` behavior).
    pub dev_user_id: String,
    /// Loopback callback port; `None` = ephemeral.
    pub callback_port: Option<u16>,
}

impl AuthConfig {
    pub fn new(edge_url: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            edge_url: edge_url.into(),
            data_dir: data_dir.into(),
            workos_client_id: None,
            workos_api_base: "https://api.workos.com".into(),
            dev_user_id: "dev-user".into(),
            callback_port: None,
        }
    }
}

/// The persisted session (refresh token + user + last org scope).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSession {
    refresh_token: String,
    user: AuthUser,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_id: Option<String>,
}

/// Access-token cache. Expiry ages the token's own lifetime (`exp - iat`) by
/// BOTH clocks, pessimistically. Monotonic alone (`Instant`) freezes across
/// system sleep (macOS `mach_absolute_time` and Linux `CLOCK_MONOTONIC` both
/// exclude suspend), so a laptop waking from hours of sleep presented a
/// wall-expired token that still read "fresh" — every room/relay redial got a
/// 401 with the same stale bearer and sync never recovered (user report).
/// Wall clock alone breaks under skewed device clocks (`exp` vs local time);
/// the elapsed-since-issue reading is skew-immune, and a BACKWARD wall step
/// (NTP correction) degrades harmlessly to the monotonic reading.
struct AccessEntry {
    token: String,
    ttl: Duration,
    got_at: Instant,
    got_wall: std::time::SystemTime,
}

impl AccessEntry {
    fn fresh(token: String) -> Self {
        let ttl = jwt_claims(&token)
            .and_then(|c| match (c.exp, c.iat) {
                (Some(exp), Some(iat)) if exp > iat => {
                    Some(Duration::from_secs((exp - iat) as u64))
                }
                _ => None,
            })
            .unwrap_or(Duration::from_secs(240));
        Self {
            token,
            ttl,
            got_at: Instant::now(),
            got_wall: std::time::SystemTime::now(),
        }
    }

    fn remaining(&self) -> Duration {
        let monotonic = self.got_at.elapsed();
        let wall = std::time::SystemTime::now()
            .duration_since(self.got_wall)
            .unwrap_or(Duration::ZERO);
        self.ttl.saturating_sub(monotonic.max(wall))
    }
}

struct AuthInner {
    config: AuthConfig,
    /// `Some(client_id)` = WorkOS mode; `None` = dev mode.
    workos: Option<String>,
    http: reqwest::Client,
    state_tx: watch::Sender<AuthState>,
    stored: Mutex<Option<StoredSession>>,
    access: Mutex<Option<AccessEntry>>,
    /// Pending sign-in `state` values (CSRF), stamped so abandoned attempts expire.
    pending: Mutex<HashMap<String, Instant>>,
    /// Single-flight refresh: WorkOS refresh tokens are single-use (rotated per
    /// exchange); two concurrent refreshes would race and could revoke the session.
    refresh_gate: tokio::sync::Mutex<()>,
    /// Loopback callback listener port, bound lazily on the first headed sign-in.
    loopback: tokio::sync::Mutex<Option<u16>>,
}

/// The auth service — cheap to clone by `Arc`.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<AuthInner>,
}

impl Auth {
    /// Build from config: dev mode unless a WorkOS client id is configured.
    pub fn new(config: AuthConfig) -> Self {
        let workos = config
            .workos_client_id
            .clone()
            .filter(|s| !s.trim().is_empty());
        let session_file = config.data_dir.join("session.json");
        let stored: Option<StoredSession> = if workos.is_some() {
            std::fs::read_to_string(&session_file)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
        } else {
            None
        };
        let initial = match (&workos, &stored) {
            (None, _) => AuthState::SignedIn {
                user: AuthUser {
                    id: config.dev_user_id.clone(),
                    email: config.dev_user_id.clone(),
                    name: None,
                },
                org_id: None,
            },
            (Some(_), Some(session)) => state_for(session.user.clone(), session.org_id.clone()),
            (Some(_), None) => AuthState::SignedOut,
        };
        let (state_tx, _) = watch::channel(initial);
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(AuthInner {
                config,
                workos,
                http,
                state_tx,
                stored: Mutex::new(stored),
                access: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                refresh_gate: tokio::sync::Mutex::new(()),
                loopback: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Like [`Auth::new`], but additionally probes `{edge}/health`: an edge running in
    /// dev auth mode forces dev mode even when a client id is configured (matching the
    /// edge's "bearer = user id" verification).
    pub async fn detect(mut config: AuthConfig) -> Self {
        if config.workos_client_id.is_some() {
            #[derive(Deserialize)]
            struct Health {
                auth: Option<String>,
            }
            let url = format!("{}/health", config.edge_url.trim_end_matches('/'));
            let probe = async {
                reqwest::Client::new()
                    .get(&url)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                    .ok()?
                    .json::<Health>()
                    .await
                    .ok()
            };
            if let Some(health) = probe.await
                && health.auth.as_deref() == Some("dev")
            {
                tracing::info!("auth: edge is in dev mode — using dev bearer");
                config.workos_client_id = None;
            }
        }
        Self::new(config)
    }

    pub fn workos_enabled(&self) -> bool {
        self.inner.workos.is_some()
    }

    /// Live auth status (current value + changes).
    pub fn watch_state(&self) -> watch::Receiver<AuthState> {
        self.inner.state_tx.subscribe()
    }

    pub fn state(&self) -> AuthState {
        self.inner.state_tx.borrow().clone()
    }

    /// The signed-in user id — the identity that scopes workspace rooms
    /// (`ws3/{orgId}/{userId}`) and local storage (`orgs/{org}/{user}/`).
    /// Dev mode mirrors the edge's dev-bearer parsing (`user@org` → `user`,
    /// a bare token IS the user id). `None` = signed out (WorkOS only).
    pub fn user_id(&self) -> Option<String> {
        if self.inner.workos.is_none() {
            let dev = &self.inner.config.dev_user_id;
            return Some(dev.split('@').next().unwrap_or(dev).to_string());
        }
        self.state().user().map(|u| u.id.clone())
    }

    /// Current bearer for edge rooms / the device relay — `None` when signed out.
    /// Dev mode: the configured user id. WorkOS: cached access token, refreshed when
    /// it has under 30s left.
    pub async fn access_token(&self) -> Option<String> {
        if self.inner.workos.is_none() {
            return Some(self.inner.config.dev_user_id.clone());
        }
        if let Some(entry) = &*lock(&self.inner.access)
            && entry.remaining() > TOKEN_SLACK
        {
            return Some(entry.token.clone());
        }
        match self.refresh(None).await {
            Ok(token) => token,
            Err(err) => {
                tracing::warn!(error = %err, "auth: refresh failed");
                None
            }
        }
    }

    /// Sleep-until-near-expiry refresh loop so long-lived dials (relay, rooms) always
    /// have a live token to present on reconnect. No-op task in dev mode.
    pub fn spawn_refresh_loop(&self) -> tokio::task::JoinHandle<()> {
        let auth = self.clone();
        tokio::spawn(async move {
            if auth.inner.workos.is_none() {
                return;
            }
            let mut state_rx = auth.watch_state();
            loop {
                if !state_rx.borrow().is_signed_in() {
                    if state_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                let remaining = lock(&auth.inner.access)
                    .as_ref()
                    .map(AccessEntry::remaining)
                    .unwrap_or(Duration::ZERO);
                let wait = remaining.saturating_sub(Duration::from_secs(60));
                if wait > Duration::ZERO {
                    // Re-evaluate at least once a minute rather than parking
                    // on one long timer: tokio timers ride the monotonic
                    // clock, which excludes system suspend — a laptop waking
                    // from sleep would otherwise wait the WHOLE original
                    // duration again before noticing the (wall-expired) token.
                    let wait = wait.min(Duration::from_secs(60));
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        changed = state_rx.changed() => {
                            if changed.is_err() { return; }
                        }
                    }
                    continue;
                }
                if let Err(err) = auth.refresh(None).await {
                    tracing::warn!(error = %err, "auth: background refresh failed");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        })
    }

    // -- sign-in flows ------------------------------------------------------

    /// Begin a headed sign-in: returns the AuthKit authorize URL redirecting to our
    /// loopback callback server (bound lazily on an ephemeral port).
    pub async fn start_sign_in(&self) -> Result<String, EngineError> {
        if self.inner.workos.is_none() {
            return Ok(String::new()); // dev mode: nothing to do (TS parity)
        }
        let port = self.ensure_loopback().await?;
        Ok(self.begin_sign_in(&format!("http://127.0.0.1:{port}/callback")))
    }

    /// Begin a headless sign-in: the redirect is the edge's hosted paste-code page —
    /// nothing ever redirects to this machine, so the browser can be anywhere.
    pub fn start_headless_sign_in(&self) -> String {
        if self.inner.workos.is_none() {
            return String::new();
        }
        let edge = self.inner.config.edge_url.trim_end_matches('/');
        self.begin_sign_in(&format!("{edge}/auth/cli/callback"))
    }

    /// Finish a headless sign-in with the pasted `state.code` string. The state half
    /// must match a sign-in started HERE (same CSRF discipline as the loopback flow).
    pub async fn complete_sign_in(&self, pasted: &str) -> Result<(), EngineError> {
        if self.inner.workos.is_none() {
            return Ok(());
        }
        let trimmed = pasted.trim();
        let (state, code) = trimmed.split_once('.').unwrap_or(("", ""));
        if state.is_empty() || code.is_empty() || !self.take_pending(state) {
            return Err(EngineError::Other(
                "invalid or expired sign-in code — start sign-in again and paste the full code"
                    .into(),
            ));
        }
        let result = self.exchange_code(code).await?;
        self.finish_sign_in(result);
        Ok(())
    }

    pub fn sign_out(&self) {
        *lock(&self.inner.stored) = None;
        *lock(&self.inner.access) = None;
        self.persist::<&StoredSession>(None);
        self.inner.state_tx.send_replace(AuthState::SignedOut);
    }

    // -- organizations ------------------------------------------------------

    pub async fn list_orgs(&self) -> Result<Vec<OrgMembership>, EngineError> {
        if self.inner.workos.is_none() {
            return Ok(Vec::new());
        }
        #[derive(Deserialize)]
        struct Orgs {
            #[serde(default)]
            orgs: Vec<OrgMembership>,
        }
        let body: Orgs = self
            .authed_json(reqwest::Method::GET, "/auth/orgs", None)
            .await?;
        Ok(body.orgs)
    }

    /// Create an org (the edge makes us its first admin member) and scope to it.
    pub async fn create_org(&self, name: &str) -> Result<(), EngineError> {
        if self.inner.workos.is_none() {
            return Ok(());
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Created {
            organization_id: String,
        }
        let created: Created = self
            .authed_json(
                reqwest::Method::POST,
                "/auth/orgs",
                Some(serde_json::json!({ "name": name })),
            )
            .await?;
        self.select_org(&created.organization_id).await
    }

    /// Scope the session to an org: one refresh with `organizationId`; the state follows
    /// the returned token's `org_id` claim.
    pub async fn select_org(&self, organization_id: &str) -> Result<(), EngineError> {
        if self.inner.workos.is_none() {
            return Ok(());
        }
        let token = self.refresh(Some(organization_id)).await?;
        let scoped = token
            .as_deref()
            .and_then(jwt_claims)
            .and_then(|c| c.org_id)
            .is_some_and(|org| org == organization_id);
        if !scoped {
            return Err(EngineError::Other(
                "could not switch to that workspace — you may no longer be a member".into(),
            ));
        }
        Ok(())
    }

    // -- internals ----------------------------------------------------------

    fn begin_sign_in(&self, redirect_uri: &str) -> String {
        let state = uuid::Uuid::new_v4().to_string();
        {
            let mut pending = lock(&self.inner.pending);
            let cutoff = Instant::now();
            pending.retain(|_, at| cutoff.duration_since(*at) < SIGN_IN_TTL);
            pending.insert(state.clone(), cutoff);
        }
        let client_id = self.inner.workos.clone().unwrap_or_default();
        format!(
            "{}/user_management/authorize?response_type=code&client_id={}&redirect_uri={}&provider=authkit&state={}",
            self.inner.config.workos_api_base.trim_end_matches('/'),
            url_encode(&client_id),
            url_encode(redirect_uri),
            state
        )
    }

    /// Consume a pending sign-in state; false when unknown/expired (CSRF check).
    fn take_pending(&self, state: &str) -> bool {
        let mut pending = lock(&self.inner.pending);
        let now = Instant::now();
        pending.retain(|_, at| now.duration_since(*at) < SIGN_IN_TTL);
        pending.remove(state).is_some()
    }

    async fn exchange_code(&self, code: &str) -> Result<SignInResult, EngineError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WireUser {
            id: String,
            email: String,
            #[serde(default)]
            first_name: Option<String>,
            #[serde(default)]
            last_name: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Exchange {
            user: WireUser,
            access_token: String,
            refresh_token: String,
        }
        let url = format!(
            "{}/auth/exchange",
            self.inner.config.edge_url.trim_end_matches('/')
        );
        let res = self
            .inner
            .http
            .post(&url)
            .json(&serde_json::json!({ "code": code }))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("the edge is unreachable: {e}")))?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "sign-in failed during token exchange ({}) — the code may have expired; start again",
                res.status().as_u16()
            )));
        }
        let body: Exchange = res
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("malformed exchange response: {e}")))?;
        let name = [body.user.first_name, body.user.last_name]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Ok(SignInResult {
            user: AuthUser {
                id: body.user.id,
                email: body.user.email,
                name: (!name.is_empty()).then_some(name),
            },
            access_token: body.access_token,
            refresh_token: body.refresh_token,
        })
    }

    fn finish_sign_in(&self, result: SignInResult) {
        let org_id = jwt_claims(&result.access_token).and_then(|c| c.org_id);
        *lock(&self.inner.access) = Some(AccessEntry::fresh(result.access_token));
        let session = StoredSession {
            refresh_token: result.refresh_token,
            user: result.user.clone(),
            org_id: org_id.clone(),
        };
        self.persist(Some(&session));
        *lock(&self.inner.stored) = Some(session);
        tracing::info!(email = %result.user.email, org = org_id.as_deref().unwrap_or("<none>"),
            "auth: signed in");
        self.inner
            .state_tx
            .send_replace(state_for(result.user, org_id));
    }

    /// Refresh the session (single-flight). `organization_id` migrates the WorkOS
    /// session to that org; routine refreshes keep the current scope. Returns the new
    /// access token, `None` when signed out / the refresh could not run.
    async fn refresh(&self, organization_id: Option<&str>) -> Result<Option<String>, EngineError> {
        let _gate = self.inner.refresh_gate.lock().await;
        // Re-check under the gate: the refresh we queued behind may have done the work.
        if organization_id.is_none()
            && let Some(entry) = &*lock(&self.inner.access)
            && entry.remaining() > TOKEN_SLACK
        {
            return Ok(Some(entry.token.clone()));
        }
        let Some(refresh_token) = lock(&self.inner.stored)
            .as_ref()
            .map(|s| s.refresh_token.clone())
        else {
            return Ok(None);
        };
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct RefreshBody<'a> {
            refresh_token: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            organization_id: Option<&'a str>,
        }
        let url = format!(
            "{}/auth/refresh",
            self.inner.config.edge_url.trim_end_matches('/')
        );
        let res = self
            .inner
            .http
            .post(&url)
            .json(&RefreshBody {
                refresh_token: &refresh_token,
                organization_id,
            })
            .send()
            .await;
        let res = match res {
            Ok(res) => res,
            Err(err) => {
                // Network failure is transient: keep the session, caller retries later.
                tracing::warn!(error = %err, "auth: refresh could not reach the edge");
                return Ok(None);
            }
        };
        let status = res.status().as_u16();
        if (400..500).contains(&status) && organization_id.is_none() {
            // A definitive 4xx means the refresh token itself is dead (revoked session,
            // deleted user) — it can NEVER succeed again. Degrade to SignedOut so every
            // downstream retry loop quiets down. (Org-switch refreshes are exempt: a 4xx
            // there means "not a member", not a dead session.)
            tracing::warn!(
                status,
                "auth: refresh rejected — session revoked; signing out"
            );
            self.sign_out();
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(EngineError::Other(format!("refresh failed ({status})")));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Tokens {
            access_token: String,
            refresh_token: String,
        }
        let tokens: Tokens = res
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("malformed refresh response: {e}")))?;
        let org_id = jwt_claims(&tokens.access_token).and_then(|c| c.org_id);
        let entry = AccessEntry::fresh(tokens.access_token.clone());
        tracing::info!(ttl_s = entry.ttl.as_secs(), "auth: access token refreshed");
        *lock(&self.inner.access) = Some(entry);
        let (user, org_changed) = {
            let mut stored = lock(&self.inner.stored);
            match stored.as_mut() {
                Some(session) => {
                    let changed = session.org_id != org_id;
                    session.refresh_token = tokens.refresh_token;
                    session.org_id = org_id.clone();
                    (session.user.clone(), changed)
                }
                None => return Ok(None), // signed out mid-refresh
            }
        };
        self.persist(lock(&self.inner.stored).as_ref());
        if org_changed {
            self.inner.state_tx.send_replace(state_for(user, org_id));
        }
        Ok(Some(tokens.access_token))
    }

    fn session_file(&self) -> PathBuf {
        self.inner.config.data_dir.join("session.json")
    }

    /// Persist (0600) or remove the stored session. Never panics: a disk error degrades
    /// to a logged warning, not a crash mid-refresh.
    fn persist<S: std::borrow::Borrow<StoredSession>>(&self, session: Option<S>) {
        let path = self.session_file();
        let outcome = match session {
            Some(session) => serde_json::to_vec(session.borrow())
                .map_err(std::io::Error::other)
                .and_then(|bytes| write_private(&path, &bytes)),
            None => match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
        };
        if let Err(err) = outcome {
            tracing::warn!(error = %err, "auth: failed to persist session");
        }
    }

    async fn authed_json<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, EngineError> {
        let token = self
            .access_token()
            .await
            .ok_or_else(|| EngineError::Other("not signed in".into()))?;
        let url = format!(
            "{}{}",
            self.inner.config.edge_url.trim_end_matches('/'),
            path
        );
        let mut req = self.inner.http.request(method, &url).bearer_auth(token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("the edge is unreachable: {e}")))?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "workspace request failed ({})",
                res.status().as_u16()
            )));
        }
        res.json::<T>()
            .await
            .map_err(|e| EngineError::Other(format!("malformed response: {e}")))
    }

    // -- loopback callback server ------------------------------------------

    /// Bind the loopback callback listener (idempotent); returns its port.
    async fn ensure_loopback(&self) -> Result<u16, EngineError> {
        let mut slot = self.inner.loopback.lock().await;
        if let Some(port) = *slot {
            return Ok(port);
        }
        let requested = self.inner.config.callback_port.unwrap_or(0);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", requested))
            .await
            .map_err(|e| EngineError::Other(format!("sign-in callback bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| EngineError::Other(format!("sign-in callback addr: {e}")))?
            .port();
        *slot = Some(port);
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(loopback_loop(listener, weak));
        tracing::info!(port, "auth: sign-in callback listening");
        Ok(port)
    }
}

struct SignInResult {
    user: AuthUser,
    access_token: String,
    refresh_token: String,
}

fn state_for(user: AuthUser, org_id: Option<String>) -> AuthState {
    // Every user must belong to an organization before the product opens up; an org-less
    // session is `NeedsOrganization`, which the UI gates on.
    match org_id {
        Some(org_id) => AuthState::SignedIn {
            user,
            org_id: Some(org_id),
        },
        None => AuthState::NeedsOrganization { user },
    }
}

/// The relay/room token seam: `Auth` IS a [`comet_rpc::TokenSource`], so the host relay
/// and link cache always dial with a fresh bearer after refreshes.
#[async_trait::async_trait]
impl comet_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        if self.inner.workos.is_some() && !self.state().is_signed_in() {
            return None;
        }
        self.access_token().await
    }
}

// ---------------------------------------------------------------------------
// Loopback HTTP (hand-rolled: no HTTP server dependency in the engine)
// ---------------------------------------------------------------------------

async fn loopback_loop(listener: tokio::net::TcpListener, inner: Weak<AuthInner>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Some(inner) = inner.upgrade() else { break };
        tokio::spawn(async move {
            if let Err(err) = handle_loopback_conn(stream, Auth { inner }).await {
                tracing::debug!(error = %err, "auth: callback connection failed");
            }
        });
    }
}

async fn handle_loopback_conn(
    mut stream: tokio::net::TcpStream,
    auth: Auth,
) -> Result<(), std::io::Error> {
    // Read the request head (bounded; we only need the request line).
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "header read"))??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or_default();
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let (status, body) = if path != "/callback" {
        ("404 Not Found", page("Not found."))
    } else {
        let params: HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), url_decode(v)))
            .collect();
        let code = params.get("code");
        let state = params.get("state");
        match (code, state) {
            (Some(code), Some(state)) if auth.take_pending(state) => {
                match auth.exchange_code(code).await {
                    Ok(result) => {
                        auth.finish_sign_in(result);
                        (
                            "200 OK",
                            page("Signed in. You can close this tab and return to Comet."),
                        )
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "auth: loopback code exchange failed");
                        (
                            "502 Bad Gateway",
                            page("Sign-in failed during token exchange — check the Comet logs."),
                        )
                    }
                }
            }
            _ => (
                "400 Bad Request",
                page("Invalid or expired sign-in link. Start again from Comet."),
            ),
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn page(message: &str) -> String {
    format!("<html><body style='font-family:sans-serif;padding:2rem'>{message}</body></html>")
}

// ---------------------------------------------------------------------------
// Small utilities (JWT claims, base64url, URL encoding, 0600 writes)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct JwtClaims {
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    iat: Option<i64>,
    #[serde(default)]
    org_id: Option<String>,
}

/// Decode (without verifying — the edge verifies) the JWT payload claims. Total: a
/// malformed token yields `None`, never a panic.
fn jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Write a file readable only by the owner (0600). On non-unix targets a plain write.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // An existing file keeps its old mode through OpenOptions — enforce 0600 anyway.
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_jwt_payload() {
        let payload = br#"{"exp":100,"iat":40,"org_id":"org_1"}"#;
        // Standard base64url without padding (as JWTs use).
        let encoded = {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in payload.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                out.push(ALPHABET[(n >> 18) as usize & 63] as char);
                out.push(ALPHABET[(n >> 12) as usize & 63] as char);
                if chunk.len() > 1 {
                    out.push(ALPHABET[(n >> 6) as usize & 63] as char);
                }
                if chunk.len() > 2 {
                    out.push(ALPHABET[n as usize & 63] as char);
                }
            }
            out
        };
        assert_eq!(
            base64url_decode(&encoded).as_deref(),
            Some(payload.as_slice())
        );
        let token = format!("h.{encoded}.sig");
        let claims = jwt_claims(&token).expect("claims decode");
        assert_eq!(claims.exp, Some(100));
        assert_eq!(claims.iat, Some(40));
        assert_eq!(claims.org_id.as_deref(), Some("org_1"));
    }

    #[test]
    fn url_coding_round_trips() {
        let raw = "http://127.0.0.1:1234/callback?x=a b&y=%";
        assert_eq!(url_decode(&url_encode(raw)), raw);
        assert_eq!(url_encode("a b"), "a%20b");
    }

    #[test]
    fn auth_state_serializes_as_proto_shape() {
        let user = AuthUser {
            id: "u1".into(),
            email: "u@x".into(),
            name: None,
        };
        let signed_in = AuthState::SignedIn {
            user: user.clone(),
            org_id: Some("org_1".into()),
        };
        let value = serde_json::to_value(&signed_in).expect("json");
        assert_eq!(
            value,
            serde_json::json!({
                "state": "signedIn",
                "user": {"id": "u1", "email": "u@x", "name": null},
                "orgId": "org_1",
            })
        );
        // The proto type itself round-trips the emitted value.
        let parsed: comet_proto::AuthState = serde_json::from_value(value).expect("proto parse");
        assert!(matches!(parsed, comet_proto::AuthState::SignedIn { .. }));
        assert_eq!(
            serde_json::to_value(AuthState::SignedOut).expect("json"),
            serde_json::json!({"state": "signedOut"})
        );
        assert_eq!(
            serde_json::to_value(AuthState::NeedsOrganization { user }).expect("json"),
            serde_json::json!({
                "state": "needsOrganization",
                "user": {"id": "u1", "email": "u@x", "name": null},
            })
        );
    }
}
