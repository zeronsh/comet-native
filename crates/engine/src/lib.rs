//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use comet_proto::HarnessId;

use comet_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser, OrgMembership};
pub use diff_sync::{CheckoutDiffSync, DiffSidecar, DiffSnapshot, capture_diff};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] comet_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] comet_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] comet_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs fully offline (sync disabled).
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$COMET_ORG_ID` or the dev default.
    /// In WorkOS mode the signed-in session's org wins.
    pub org_id: Option<String>,
    /// WorkOS client id — enables real auth; `None` = dev mode (bearer = `edge_token`).
    pub workos_client_id: Option<String>,
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<comet_rpc::LinkCache>>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Identity comes from
    /// `$COMET_ORG_ID` / `$COMET_USER_ID` (dev defaults `dev-org` / `dev-user`);
    /// use [`Self::assemble_with_identity`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let org_id = env_or("COMET_ORG_ID", DEFAULT_ORG_ID);
        let user_id = env_or("COMET_USER_ID", DEFAULT_USER_ID);
        Self::assemble_with_identity(data_dir, registry, default_harness, edge, &org_id, &user_id)
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        // Identity-scoped storage: snapshots, the command ledger, and run
        // journals live under `orgs/{orgId}/{userId}/` so switching accounts or
        // orgs on one machine never reuses another identity's cached docs.
        let org_dir = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        let store = Arc::new(DocsStore::open(&org_dir)?);
        let journal = Arc::new(RunJournal::open(org_dir.join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                edge: edge.clone(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir, edge.clone());
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            device_id,
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached auth service, or a lazily-created dev-mode one (in-process embeds
    /// that never wired WorkOS still answer AuthStatus honestly).
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let dev_user = std::env::var("COMET_EDGE_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dev-user".into());
            let mut config = AuthConfig::new("http://localhost:27640", std::env::temp_dir());
            config.dev_user_id = dev_user;
            Auth::new(config)
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing and [`Self::dial_device`].
    pub fn set_links(&self, links: Arc<comet_rpc::LinkCache>) {
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<comet_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<comet_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> comet_rpc::HostRelay {
        let auth = self.auth();
        let config =
            comet_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));
        let doc_host = self.doc_host.clone();
        let on_nudge: comet_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            // Opening the doc joins its room + syncs; drain fires on the change
            // subscription — the command executes with no standing per-chat socket.
            match doc_host.open(&chat_id) {
                Ok(_) => tracing::info!(chat = %chat_id, "nudge: chat doc opened"),
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "nudge: open failed")
                }
            }
        });
        comet_rpc::HostRelay::spawn(config, self.rpc_service(), on_nudge)
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
        )
        .with_auth(self.auth());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        Arc::new(rpc)
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Run until ctrl-c: auth (dev or WorkOS), sessions engine + doc host + command
    /// executor, IPC server, and — when edge+auth are ready — the device-room host
    /// relay + peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let mut auth_config = AuthConfig::new(config.edge_url.clone(), config.data_dir.clone());
        auth_config.workos_client_id = config.workos_client_id.clone();
        if let Ok(base) = std::env::var("COMET_WORKOS_API_BASE")
            && !base.trim().is_empty()
        {
            auth_config.workos_api_base = base;
        }
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        let auth = Auth::detect(auth_config).await;
        let _refresh_loop = auth.spawn_refresh_loop();

        // WorkOS mode: gate edge features on a signed-in, org-scoped session. Headless
        // sign-in prompt on TTY (paste-code flow) — CompleteSignIn over IPC also works.
        if auth.workos_enabled() {
            wait_for_sign_in(&auth).await;
        }

        // Edge sync: enabled when signed in (WorkOS) or a dev bearer is configured;
        // `None` runs fully offline (no rooms, no relay) — M2 behavior preserved.
        // The config carries `Auth` as a token PROVIDER, not a snapshot: every room
        // (re)connect and edge request re-reads the (refreshed) access token, so an
        // expired WorkOS token is never presented after a socket drop.
        let online = (auth.workos_enabled() || config.edge_token.is_some())
            && auth.access_token().await.is_some();
        let edge = online.then(|| EdgeConfig::new(config.edge_url.clone(), Arc::new(auth.clone())));

        // Workspace identity: the session's org claim (WorkOS) beats the dev
        // bearer's `user@org` suffix beats the configured org; the user id
        // comes from the signed-in session (dev mode: the bearer's prefix,
        // mirroring the edge's parsing — the edge derives BOTH from the token,
        // so the token must win over env or the room join 403s). Everything
        // downstream — the per-user `ws3/{org}/{user}` workspace room and the
        // org/user-scoped local store — keys off this pair.
        let dev_token_org = config
            .edge_token
            .as_deref()
            .and_then(|t| t.split_once('@'))
            .map(|(_, org)| org.to_string())
            .filter(|s| !s.is_empty());
        let org_id = auth
            .state()
            .org_id()
            .map(str::to_string)
            .or(dev_token_org)
            .or(config.org_id.clone())
            .unwrap_or_else(|| env_or("COMET_ORG_ID", DEFAULT_ORG_ID));
        let user_id = auth
            .user_id()
            .unwrap_or_else(|| env_or("COMET_USER_ID", DEFAULT_USER_ID));
        let core = EngineCore::assemble_with_identity(
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            edge.clone(),
            &org_id,
            &user_id,
        )?;
        core.set_auth(auth.clone());
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        // Device-room transport (edge + auth ready): host our relay room and enable
        // peer dialing. Token refreshes take effect on every (re)dial via `Auth`'s
        // TokenSource impl.
        let _host_relay = edge.as_ref().map(|edge| {
            let links = comet_rpc::LinkCache::new(comet_rpc::LinkCacheConfig::new(
                edge.url.clone(),
                Arc::new(auth.clone()),
            ));
            // Data-driven cooldown reset: a peer whose workspace presence
            // heartbeat is fresh is reachable — clear its dial backoff so
            // interactive remote control recovers immediately after a blip.
            let links_for_presence = links.clone();
            core.workspace
                .set_peer_alive_hook(Arc::new(move |device_id: &str| {
                    links_for_presence.reset_cooldown(device_id);
                }));
            core.set_links(links);
            core.start_host_relay(&edge.url)
        });

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.ipc_port)).await?;
        tracing::info!(port = config.ipc_port, "IPC server listening");
        let server = tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");
        server.abort();
        core.shutdown().await;
        Ok(())
    }
}

/// Block until the WorkOS session is signed in AND org-scoped. On a TTY, print the
/// headless (paste-code) sign-in URL and read the pasted `state.code` from stdin;
/// `SignIn`/`CompleteSignIn`/`CreateOrg` over IPC drive the same state.
async fn wait_for_sign_in(auth: &Auth) {
    use std::io::IsTerminal;
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    let mut org_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let state = state_rx.borrow().clone();
        match state {
            AuthState::SignedIn { user, org_id } => {
                tracing::info!(email = %user.email, org = org_id.as_deref().unwrap_or("<none>"),
                    "auth: session ready");
                break;
            }
            AuthState::NeedsOrganization { user } => {
                if org_reader.is_none() {
                    if std::io::stdin().is_terminal() {
                        // Workspace onboarding on the TTY (old comet's
                        // `backend login` flow): create if none, auto-join a
                        // single membership, numbered picker otherwise.
                        println!("Signed in as {}.", user.email);
                        org_reader = Some(tokio::spawn(run_org_onboarding(auth.clone())));
                    } else {
                        println!(
                            "Signed in as {} — create or select a workspace from the Comet UI to continue.",
                            user.email
                        );
                    }
                }
            }
            AuthState::SignedOut => {
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in();
                    println!("Sign in to Comet:\n\n  {url}\n");
                    if std::io::stdin().is_terminal() {
                        println!("Then paste the code shown in the browser here and press enter.");
                        let auth = auth.clone();
                        stdin_reader = Some(tokio::spawn(async move {
                            loop {
                                let Some(line) = read_stdin_line().await else {
                                    return;
                                };
                                let pasted = line.trim();
                                if pasted.is_empty() {
                                    continue;
                                }
                                match auth.complete_sign_in(pasted).await {
                                    Ok(()) => return,
                                    Err(err) => println!("Sign-in failed: {err}"),
                                }
                            }
                        }));
                    }
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    if let Some(reader) = org_reader {
        reader.abort();
    }
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// TTY workspace onboarding for an org-less session (ports old comet's
/// `backend login` flow): no memberships → prompt a name and create; exactly
/// one → auto-join; several → numbered picker. Success flips the auth state to
/// `SignedIn`, which ends [`wait_for_sign_in`]'s wait (and aborts this task).
async fn run_org_onboarding(auth: Auth) {
    let orgs = match auth.list_orgs().await {
        Ok(orgs) => orgs,
        Err(err) => {
            println!(
                "Could not list workspaces ({err}) — create or select one from the Comet UI to continue."
            );
            return;
        }
    };
    match orgs.len() {
        0 => {
            println!("No workspaces yet — name your new workspace and press enter:");
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let name = line.trim();
                if name.is_empty() {
                    continue;
                }
                match auth.create_org(name).await {
                    Ok(()) => return,
                    Err(err) => println!("Creating workspace failed: {err}"),
                }
            }
        }
        1 => {
            let only = &orgs[0];
            println!("Joining workspace \"{}\"…", only.name);
            if let Err(err) = auth.select_org(&only.organization_id).await {
                println!("Joining workspace failed: {err}");
            }
        }
        _ => {
            println!("\nYour workspaces:");
            for (index, org) in orgs.iter().enumerate() {
                println!("  {}. {}", index + 1, org.name);
            }
            println!("Pick a workspace [1-{}]:", orgs.len());
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let choice = line
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|index| orgs.get(index));
                let Some(org) = choice else {
                    println!("Pick a workspace [1-{}]:", orgs.len());
                    continue;
                };
                match auth.select_org(&org.organization_id).await {
                    Ok(()) => return,
                    Err(err) => println!("Joining workspace failed: {err}"),
                }
            }
        }
    }
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("COMET_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

/// Trimmed env var or the given default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Filesystem-safe form of an org/user id (path segments for `orgs/{org}/{user}/`).
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}
