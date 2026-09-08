//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring zeron: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use crate::comments::ReviewComment;
use zeron_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use zeron_engine::{Engine, EngineConfig, EngineRuntime, InstanceLock, rpc::AuthRpc};
use zeron_proto::{
    AuthState, ChangeRequestSummary, Chat, ChatIndicator, CheckoutChangeRequestStatus, Device,
    EngineInfo, HarnessId, Session, Space, WorkspaceScope,
};
use zeron_rpc::{RpcClient, RpcError, RpcReply, RpcService, connect_ws, memory_client, methods};

use crate::change_requests::{
    ChangeRequestClientState, ChangeRequestWatchKey, desired_watch_targets, watch_params,
};

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.zeron`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// WorkOS client id for production authentication.
    pub workos_client_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    boot_task: tokio::task::JoinHandle<()>,
    refresh_task: tokio::task::JoinHandle<()>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        self.boot_task.abort();
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
        self.refresh_task.abort();
    }
}

#[derive(Clone)]
enum DeferredEngineState {
    Waiting,
    Ready,
    Failed(String),
}

/// Serves engine identity and AuthRpc immediately, then holds data calls only
/// while a captured synced profile still needs organization onboarding.
/// Existing subscriptions attach to the assembled service without reconnecting.
struct DeferredEngineRpc {
    auth: AuthRpc,
    engine_info: EngineInfo,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
    service: Arc<tokio::sync::OnceCell<Arc<dyn RpcService>>>,
}

#[async_trait]
impl RpcService for DeferredEngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == methods::ENGINE_INFO {
            return RpcReply::value(&self.engine_info);
        }
        if method == methods::ENGINE_READY {
            let mut state = self.state.clone();
            return match wait_for_deferred_engine(&mut state).await {
                Ok(()) => RpcReply::value(&serde_json::json!({ "ready": true })),
                Err(message) => Err(RpcError::Failed(message)),
            };
        }
        if AuthRpc::handles(method) {
            return self.auth.handle(method, params).await;
        }

        let mut state = self.state.clone();
        loop {
            let current = { state.borrow().clone() };
            match current {
                DeferredEngineState::Waiting => {}
                DeferredEngineState::Ready => {
                    let service = self.service.get().ok_or_else(|| {
                        RpcError::Failed(
                            "embedded engine became ready without an RPC service".into(),
                        )
                    })?;
                    return service.handle(method, params).await;
                }
                DeferredEngineState::Failed(message) => return Err(RpcError::Failed(message)),
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

async fn wait_for_deferred_engine(
    state: &mut tokio::sync::watch::Receiver<DeferredEngineState>,
) -> Result<(), String> {
    loop {
        let current = { state.borrow().clone() };
        match current {
            DeferredEngineState::Waiting => {}
            DeferredEngineState::Ready => return Ok(()),
            DeferredEngineState::Failed(message) => return Err(message),
        }
        state
            .changed()
            .await
            .map_err(|_| "embedded engine assembly ended without a result".to_string())?;
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: Arc<RpcClient>,
    url: String,
    lifecycle_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; only stop our readiness probe.
        if let Some(task) = self.lifecycle_task.lock().await.take() {
            task.abort();
        }
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
    engine_info: EngineInfo,
    deferred_state: Option<tokio::sync::watch::Receiver<DeferredEngineState>>,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        // Invariant: at most one bootstrap in this process runs probe+embed at
        // a time. The winner binds the deferred IPC listener before releasing
        // the gate, so a concurrent viewport's probe finds it and attaches as
        // Remote instead of racing it for the data dir.
        static BOOTSTRAP_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _gate = BOOTSTRAP_GATE.lock().await;

        if let Some(handle) = Self::attach_to_daemon(config.ipc_port).await {
            return Ok(handle);
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            org_id: config.org_id,
            workos_client_id: config.workos_client_id,
        };

        // Own the data dir before opening anything under it or binding IPC —
        // the lock, not the port bind, is the ownership decision. A failed
        // acquire means an out-of-process engine holds the dir but was not
        // serving IPC at probe time (a daemon mid-start): wait for its
        // listener, re-trying the lock in case it dies instead.
        std::fs::create_dir_all(&engine_config.data_dir)?;
        let lock_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let lock = loop {
            match InstanceLock::acquire(&engine_config.data_dir) {
                Ok(lock) => break lock,
                Err(err) => {
                    if std::time::Instant::now() >= lock_deadline {
                        return Err(err.into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if let Some(handle) = Self::attach_to_daemon(engine_config.ipc_port).await {
                        return Ok(handle);
                    }
                }
            }
        };

        let auth = Engine::build_auth(&engine_config).await;
        let workspace_scope = Engine::initial_workspace_scope(&auth);
        let initial_profile = Engine::resolve_profile(&engine_config, &auth, workspace_scope)?;
        let profile_is_resolved = initial_profile.is_some();
        let engine_info = Engine::engine_info(&engine_config, workspace_scope)?;
        let refresh_task = auth.spawn_refresh_loop();
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let assembled_service = Arc::new(tokio::sync::OnceCell::new());
        let service: Arc<dyn RpcService> = Arc::new(DeferredEngineRpc {
            auth: AuthRpc::new(auth.clone()),
            engine_info: engine_info.clone(),
            state: state_rx.clone(),
            service: assembled_service.clone(),
        });
        let client = memory_client(service.clone());

        // Serve the same service on the IPC port so a terminal viewport can
        // attach to this window's engine with no setup. Deliberately the
        // *deferred* service, not the assembled one: a viewport that connects
        // during cloud onboarding gets EngineInfo and AuthRpc immediately, and
        // its data subscriptions wait exactly as this window's do.
        //
        // Best-effort — losing the bind race with another engine costs other
        // viewports, not this one.
        let ipc_task = match zeron_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(None));
        let runtime_for_boot = runtime.clone();
        let service_for_boot = assembled_service.clone();
        // The instance lock rides into the boot task and is consumed by
        // assembly — held through sign-in onboarding too, because this process
        // owns the data dir from the moment it decided to embed.
        let boot_task = tokio::spawn(async move {
            let profile = match initial_profile {
                Some(profile) => profile,
                None => {
                    let mut auth_state = auth.watch_state();
                    while !auth_state.borrow().is_signed_in() {
                        if auth_state.changed().await.is_err() {
                            state_tx.send_replace(DeferredEngineState::Failed(
                                "authentication state closed before workspace onboarding".into(),
                            ));
                            return;
                        }
                    }
                    match Engine::resolve_profile(&engine_config, &auth, workspace_scope) {
                        Ok(Some(profile)) => profile,
                        Ok(None) => {
                            state_tx.send_replace(DeferredEngineState::Failed(
                                "workspace onboarding completed without an organization".into(),
                            ));
                            return;
                        }
                        Err(err) => {
                            state_tx.send_replace(DeferredEngineState::Failed(err.to_string()));
                            return;
                        }
                    }
                }
            };

            match Engine::assemble_runtime_with_lock(&engine_config, auth, profile, lock).await {
                Ok(engine_runtime) => {
                    let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
                    *runtime_for_boot.lock().await = Some(engine_runtime);
                    if service_for_boot.set(service).is_err() {
                        state_tx.send_replace(DeferredEngineState::Failed(
                            "embedded engine RPC service was assembled more than once".into(),
                        ));
                        return;
                    }
                    state_tx.send_replace(DeferredEngineState::Ready);
                }
                Err(err) => {
                    tracing::error!(error = %err, "embedded engine assembly failed");
                    state_tx.send_replace(DeferredEngineState::Failed(format!("{err:#}")));
                }
            }
        });
        let handle = EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                boot_task,
                refresh_task,
                ipc_task,
                client,
            }),
            engine_info,
            deferred_state: Some(state_rx.clone()),
        };
        // Local, development, and already-resolved synced profiles need no
        // authentication UI while assembling. Keep the viewport Connecting
        // until their stores and journals are actually open, and surface a
        // boot failure through the existing bootstrap error path.
        if profile_is_resolved && let Err(message) = wait_for_deferred_engine(&mut state_rx).await {
            handle.shutdown().await;
            return Err(anyhow::anyhow!(message));
        }
        Ok(handle)
    }

    /// Probe the IPC port and, if a live engine answers, attach as a remote
    /// viewport. `None` means embed: nothing listening, a non-engine listener,
    /// or a listener without an identity.
    async fn attach_to_daemon(ipc_port: u16) -> Option<EngineHandle> {
        let url = format!("ws://127.0.0.1:{ipc_port}");
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
        )
        .await;
        if !matches!(probe, Ok(Ok(_))) {
            return None;
        }
        tracing::info!(%url, "engine daemon detected; connecting");
        match connect_ws(&url).await {
            Ok(client) => match query_engine_info(&client).await {
                Ok(engine_info) => {
                    let client = Arc::new(client);
                    let (state_tx, state_rx) =
                        tokio::sync::watch::channel(DeferredEngineState::Waiting);
                    let lifecycle_client = client.clone();
                    let lifecycle_task = tokio::spawn(async move {
                        let state = match lifecycle_client
                            .call(methods::ENGINE_READY, serde_json::json!({}))
                            .await
                        {
                            Ok(_) => DeferredEngineState::Ready,
                            // EngineReady was added after EngineInfo. An older daemon
                            // that does not expose the barrier is already assembled.
                            Err(RpcError::UnknownMethod(method))
                                if method == methods::ENGINE_READY =>
                            {
                                DeferredEngineState::Ready
                            }
                            Err(err) => DeferredEngineState::Failed(err.to_string()),
                        };
                        state_tx.send_replace(state);
                    });
                    Some(EngineHandle {
                        inner: Arc::new(RemoteEngine {
                            client,
                            url,
                            lifecycle_task: tokio::sync::Mutex::new(Some(lifecycle_task)),
                        }),
                        engine_info,
                        deferred_state: Some(state_rx),
                    })
                }
                Err(err) => {
                    tracing::warn!(
                        %url,
                        error = %err,
                        "listener did not provide engine identity; embedding instead"
                    );
                    None
                }
            },
            // Something is on the port but it is not an engine (or it is
            // wedged). Fall through and embed: a stranger holding 27654
            // should cost other viewports, not this window.
            Err(err) => {
                tracing::warn!(%url, error = %err, "not an engine; embedding instead");
                None
            }
        }
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    fn deferred_state(&self) -> Option<tokio::sync::watch::Receiver<DeferredEngineState>> {
        self.deferred_state.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

/// Query the current protocol first, with a conservative fallback for daemons
/// from before `EngineInfo` existed. Old daemons are always treated as synced.
async fn query_engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
    match client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
    {
        Ok(info) => Ok(info),
        Err(RpcError::UnknownMethod(method)) if method == methods::ENGINE_INFO => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct LocalDevice {
                device_id: String,
            }
            let legacy: LocalDevice = client
                .call_as(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await?;
            Ok(EngineInfo {
                device_id: legacy.device_id,
                workspace_scope: WorkspaceScope::Synced,
            })
        }
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `zeron_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use zeron_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    parse_auth_state, project_label, sort_active, sort_chats, sort_spaces, sort_tabs,
};

// ---------------------------------------------------------------------------
// Org gate (pure)
// ---------------------------------------------------------------------------

/// One org membership row (tolerant local mirror of the engine's ListOrgs
/// reply — `{orgs: [{id, organizationId, name}]}`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRow {
    pub organization_id: String,
    pub name: String,
}

/// Parse a ListOrgs reply tolerantly (accepts a bare array too).
pub fn parse_orgs(value: &serde_json::Value) -> Vec<OrgRow> {
    let list = value.get("orgs").unwrap_or(value);
    serde_json::from_value(list.clone()).unwrap_or_default()
}

/// Workspace names must be non-empty (trimmed) and reasonably short.
pub fn org_name_valid(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 64
}

/// Memberships sorted by name (case-insensitive), deduped by organization id.
pub fn sort_memberships(mut orgs: Vec<OrgRow>) -> Vec<OrgRow> {
    orgs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    orgs.dedup_by(|a, b| a.organization_id == b.organization_id);
    orgs
}

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// A composer send whose doc command is queued but not yet executed by the
/// chat's host device — cleared when the host writes the user message back
/// into the transcript (same client-minted id as the [`AppState::echoes`]
/// dedup); past [`UNDELIVERED_GRACE_MS`] it surfaces as the explicit
/// failed/retry state instead.
#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started: DateTime<Utc>,
}

/// How long an unadopted send reads as Working/Sending (or Queued when the
/// path is degraded) before flipping to the EXPLICIT failed state with a
/// retry affordance. The old 30s overlay silently expired back to Idle with
/// no visible trace of the send at all — the exact hole the 2026-08-19
/// incident fell into.
pub const UNDELIVERED_GRACE_MS: i64 = 120_000;

/// A send's attachment-upload leg in flight. `done` is bumped by the upload
/// task per completed chunk (binary bytes); the working label reads it every
/// paint (the spinner already animates each frame), so no notify plumbing is
/// needed. A slow upload renders as "Uploading… N%" instead of a
/// hang-indistinguishable "Sending…" (2026-08-18 user report).
pub struct UploadProgress {
    done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    total: u64,
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Fixed data boundary of the attached engine. Authentication may change
    /// in place, but changing this scope requires assembling a new runtime.
    pub workspace_scope: Option<WorkspaceScope>,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    // Last published device presentation. Heartbeats refresh the underlying
    // timestamps without invalidating every view when no displayed value changed.
    device_presentation: Option<Vec<(Device, bool, String)>>,
    // Presence ticks also retire stale remote session indicators. Remember
    // their last published appearance even when the device rows stay online.
    session_presence_presentation: Vec<Indicator>,
    /// Live edge posture (WatchConnectivity): drives the connection pill,
    /// composer honesty ("will queue"), and the Queued send badges.
    pub connectivity: zeron_proto::Connectivity,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    session_presentation: Option<Vec<Session>>,
    /// The project the new-session canvas mints into. Healed by
    /// [`Self::apply_spaces`] when the row vanishes; selecting a chat implies
    /// its project.
    pub selected_space: Option<String>,
    /// Deliberate "Don't work in a project" pick: while set, the canvas mints
    /// project-less sessions (cwd `~` on the picked device) and
    /// [`Self::selected_space_row`] reads as `None` — healing must NOT
    /// re-select a project underneath it.
    pub no_project: bool,
    /// The composer's device pick — where project-less sessions run, and the
    /// device whose projects the project picker lists. `None` falls back to
    /// the local device.
    pub selected_device: Option<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// First chats / spaces watch frame has landed — device-local state that
    /// prunes against the doc (open tabs, the sidebar space filter) must not
    /// judge by the empty pre-sync lists.
    pub chats_synced: bool,
    pub spaces_synced: bool,
    pending_deep_link: Option<crate::links::ConversationDeepLink>,
    deep_link_notice: Option<String>,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    pub context_usage: Option<zeron_proto::ContextUsage>,
    /// The selected chat's opening `WatchDocMessages` reset has landed. An
    /// empty transcript is otherwise indistinguishable from the pre-replay
    /// gap after selection, where optimistic echoes may already be visible.
    pub transcript_replayed: bool,
    /// Changes only when transcript/optimistic content changes. Presence,
    /// catalogs and other app-state notifications need no row derivation.
    pub(crate) transcript_revision: u64,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// Send-in-flight overlay per chat id: a queued doc command the host
    /// hasn't executed yet (see [`Self::begin_pending_send`]).
    pending_sends: HashMap<String, PendingSend>,
    /// The in-flight send's attachment upload, when it has one.
    upload_progress: Option<UploadProgress>,
    /// Engine-side queued-attachment transfers by uploadId (`WatchTransfers`
    /// snapshots): real relay-leg progress for the sending thumbnail's
    /// percent ring, present exactly while bytes are moving.
    transfers: HashMap<String, (u64, u64)>,
    /// Written by the changes pane, read by the composer.
    review_comments: HashMap<String, Vec<ReviewComment>>,
    /// File surfaces whose editor-backed comments currently cite a buffer
    /// revision that has not reached disk yet. A chat remains blocked until
    /// every surface waiting on a workspace write has finished or cancelled.
    review_comment_flushes: HashMap<String, HashSet<u64>>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<zeron_update::UpdateStatus>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    engine: Option<EngineHandle>,
    watch_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
    change_requests: ChangeRequestClientState,
    change_request_tasks: HashMap<ChangeRequestWatchKey, Task<()>>,
    change_requests_visible: bool,
    /// SUBAGENT transcripts keyed by subagent doc id (the right pane's
    /// subagent tabs read these). Independent of `selected_chat`: a tab's
    /// feed must survive chat switches — the tab itself is what scopes it.
    sub_transcripts: HashMap<String, Vec<SessionMessageEntry>>,
    /// One watch task per live subagent doc (single-flight per key).
    /// Dropping a task cancels the engine-side watch and unpins the doc from
    /// the engine LRU — closing a tab MUST go through
    /// [`Self::unwatch_subagent_doc`].
    sub_watch_tasks: HashMap<String, Task<()>>,
}

/// Text/reasoning growth changes the transcript without changing session
/// status, tools, navigation, or composer state. Keep those frames local to
/// the affected transcript instead of rebuilding every app-state observer.
pub(crate) struct TranscriptTextChanged {
    pub doc_id: String,
}

impl gpui::EventEmitter<TranscriptTextChanged> for AppState {}

fn is_text_append(frame: &TranscriptFrame) -> bool {
    matches!(frame, TranscriptFrame::Delta { upsert, append, remove, .. }
        if upsert.is_empty() && remove.is_empty() && !append.is_empty())
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            workspace_scope: None,
            auth: None,
            devices: Vec::new(),
            device_presentation: None,
            session_presence_presentation: Vec::new(),
            connectivity: zeron_proto::Connectivity::default(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            session_presentation: None,
            selected_space: None,
            no_project: false,
            selected_device: None,
            selected_chat: None,
            transcript: Vec::new(),
            context_usage: None,
            transcript_replayed: false,
            transcript_revision: 0,
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            upload_progress: None,
            transfers: HashMap::new(),
            review_comments: HashMap::new(),
            review_comment_flushes: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: None,
            engine: None,
            watch_tasks: Vec::new(),
            transcript_task: None,
            change_requests: ChangeRequestClientState::default(),
            change_request_tasks: HashMap::new(),
            change_requests_visible: true,
            sub_transcripts: HashMap::new(),
            sub_watch_tasks: HashMap::new(),
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
            pending_deep_link: None,
            deep_link_notice: None,
        }
    }

    /// The selected chat, or `""` on the new-chat canvas. Identical to the
    /// composer's own attachment/draft key, so a comment written before the
    /// first send survives the chat being minted.
    pub fn composer_key(&self) -> String {
        self.selected_chat.clone().unwrap_or_default()
    }

    pub fn review_comments(&self, key: &str) -> &[ReviewComment] {
        self.review_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, key: &str, comment: ReviewComment) {
        self.review_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_review_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.review_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.review_comments.remove(key);
                self.review_comment_flushes.remove(key);
            }
        }
    }

    pub fn update_review_comment_line(&mut self, key: &str, id: &str, line: u32) {
        if let Some(comment) = self
            .review_comments
            .get_mut(key)
            .and_then(|comments| comments.iter_mut().find(|comment| comment.id == id))
        {
            comment.line = line;
        }
    }

    pub fn rename_review_comment_path(&mut self, key: &str, old_path: &str, new_path: &str) {
        if let Some(comments) = self.review_comments.get_mut(key) {
            for comment in comments
                .iter_mut()
                .filter(|comment| comment.is_file() && comment.path == old_path)
            {
                comment.path = new_path.to_string();
            }
        }
    }

    pub fn take_review_comments(&mut self, key: &str) -> Vec<ReviewComment> {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_review_comments(&mut self, key: &str) {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key);
    }

    pub fn begin_review_comment_flush(&mut self, key: &str, source: u64) {
        self.review_comment_flushes
            .entry(key.to_string())
            .or_default()
            .insert(source);
    }

    pub fn finish_review_comment_flush(&mut self, key: &str, source: u64) {
        let remove_key = self
            .review_comment_flushes
            .get_mut(key)
            .is_some_and(|sources| {
                sources.remove(&source);
                sources.is_empty()
            });
        if remove_key {
            self.review_comment_flushes.remove(key);
        }
    }

    pub fn review_comment_flush_pending(&self, key: &str) -> bool {
        self.review_comment_flushes.contains_key(key)
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.transcript.clear();
            self.context_usage = None;
            self.transcript_revision = self.transcript_revision.wrapping_add(1);
            self.transcript_replayed = false;
            self.transcript_task = None;
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) -> bool {
        self.apply_sessions_at(sessions, Utc::now())
    }

    fn apply_sessions_at(&mut self, sessions: Vec<Session>, now: DateTime<Utc>) -> bool {
        let presence: Vec<_> = sessions
            .iter()
            .map(|session| effective_indicator(Some(session), now))
            .collect();
        let presentation: Vec<_> = sessions
            .iter()
            .map(|session| {
                let mut metadata = session.clone();
                // The timestamp is a liveness lease, not visible text. Keep its
                // effective indicator in the key and retain the actual value below.
                metadata.updated_at = DateTime::<Utc>::UNIX_EPOCH;
                metadata
            })
            .collect();
        let changed = self.session_presentation.as_ref() != Some(&presentation)
            || self.session_presence_presentation != presence;
        self.session_presentation = Some(presentation);
        self.session_presence_presentation = presence;
        self.sessions = sessions;
        changed
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        // Heal a vanished selection (project deleted elsewhere): fall back to
        // the first project; its chats died with it, so a matching chat
        // selection is healed by the accompanying chats frame (`apply_chats`).
        // The picker lists projects per-device, so healing prefers one on the
        // picked device — a global fallback would silently re-aim the canvas
        // at another machine.
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.first_space_on_picked_device();
        }
        // First frame with no selection yet: pick the first project so the
        // canvas never boots project-less by accident — unless the user
        // deliberately opted out.
        if self.selected_space.is_none() && !self.no_project {
            self.selected_space = self.first_space_on_picked_device();
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: zeron_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_connectivity(&mut self, connectivity: zeron_proto::Connectivity) {
        self.connectivity = connectivity;
    }

    /// Is this chat's delivery path degraded — will a send QUEUE rather than
    /// reach its executor promptly? Locally-hosted chats are never degraded
    /// (a queued command executes on this device even fully offline). Remote
    /// chats degrade when the OS says offline, when the chat's own edge room
    /// is down, or when the host device has gone presence-dark.
    pub fn chat_delivery_degraded(&self, chat_id: &str) -> bool {
        use zeron_proto::ConnectivityState as S;
        if self.connectivity.state == S::Disabled {
            return false;
        }
        let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) else {
            // Unknown chat (a just-minted canvas send): only the global
            // state can speak.
            return self.connectivity.state == S::Offline;
        };
        if Some(chat.device_id.as_str()) == self.local_device_id.as_deref() {
            return false;
        }
        if self.connectivity.state == S::Offline {
            return true;
        }
        let room_down = match self
            .connectivity
            .chats
            .iter()
            .find(|c| c.chat_id == chat_id)
        {
            Some(net) => !net.connected,
            None => self.connectivity.state != S::Connected,
        };
        room_down || !self.device_online(&chat.device_id, Utc::now())
    }

    /// A send is queued: in flight AND its delivery path is degraded — the
    /// honest badge is "Queued", not a Working spinner.
    pub fn send_queued(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.send_pending(chat_id, now) && self.chat_delivery_degraded(chat_id)
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) -> bool {
        self.apply_devices_at(devices, Utc::now())
    }

    fn apply_devices_at(&mut self, mut devices: Vec<Device>, now: DateTime<Utc>) -> bool {
        // A local-only workspace has no remote device identity to distinguish.
        // Keep the engine's legacy sentinel out of the UI while preserving real
        // hostnames and user-assigned device names.
        if self.workspace_scope == Some(WorkspaceScope::Local)
            && let Some(local_id) = self.local_device_id.as_deref()
            && let Some(device) = devices.iter_mut().find(|device| device.id == local_id)
            && device.name == "unknown-device"
        {
            device.name = "Local".to_string();
        }
        for device in &devices {
            self.change_requests
                .clear_unsupported_on_version_change(&device.id, device.version.as_deref());
        }
        let presentation: Vec<_> = devices
            .iter()
            .map(|device| {
                let mut metadata = device.clone();
                metadata.last_seen_at = None;
                (
                    metadata,
                    crate::settings::devices::device_online(device.last_seen_at, now),
                    crate::settings::devices::format_last_seen(device.last_seen_at, now),
                )
            })
            .collect();
        let session_presence: Vec<_> = self
            .sessions
            .iter()
            .map(|session| effective_indicator(Some(session), now))
            .collect();
        let changed = self.device_presentation.as_ref() != Some(&presentation)
            || self.session_presence_presentation != session_presence;
        self.device_presentation = Some(presentation);
        self.session_presence_presentation = session_presence;
        // Freshness must advance even when the heartbeat does not redraw:
        // delivery gating and later renders still need the newest timestamp.
        self.devices = devices;
        changed
    }

    /// True when `device_id`'s engine (per its registry device row) is at
    /// least `min`. Unknown devices and unstamped versions are conservatively
    /// false — feature gates fall back to the legacy path rather than speak a
    /// protocol the peer may not understand.
    pub fn device_version_at_least(&self, device_id: &str, min: (u64, u64, u64)) -> bool {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .and_then(|d| d.version.as_deref())
            .and_then(version_triple)
            .is_some_and(|v| v >= min)
    }

    /// First project on the composer's picked device (falling back through
    /// the local device, then any project at all — better a cross-device
    /// project than a surprise project-less canvas). Display order.
    fn first_space_on_picked_device(&self) -> Option<String> {
        let device = self
            .selected_device
            .as_deref()
            .or(self.local_device_id.as_deref());
        let sorted = self.spaces_sorted();
        device
            .and_then(|d| sorted.iter().find(|s| s.device_id == d).copied())
            .or_else(|| sorted.first().copied())
            .map(|s| s.id.clone())
    }

    pub fn apply_update(&mut self, status: zeron_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&zeron_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.transcript_replayed = true;
        self.ack_pending_send_from_transcript();
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        let is_reset = matches!(&frame, TranscriptFrame::Reset { .. });
        zeron_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if is_reset {
            self.transcript_replayed = true;
        }
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.ack_pending_send_from_transcript();
        Ok(())
    }

    /// Apply an incoming frame and invalidate only its affected presentation.
    pub fn receive_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
        cx: &mut Context<Self>,
    ) -> Result<(), TranscriptDesync> {
        let text_doc = self
            .selected_chat
            .as_ref()
            .filter(|id| {
                is_text_append(&frame)
                    && !self.pending_sends.contains_key(*id)
                    && self.pending_echoes().is_empty()
            })
            .cloned();
        let result = self.apply_transcript_frame(frame);
        if let Some(doc_id) = text_doc.filter(|_| result.is_ok()) {
            cx.emit(TranscriptTextChanged { doc_id });
        } else {
            cx.notify();
        }
        result
    }

    /// A subagent doc's current transcript copy (empty until its watch's
    /// replay frame lands, or its frozen snapshot is set).
    pub fn sub_transcript(&self, doc_id: &str) -> &[SessionMessageEntry] {
        self.sub_transcripts
            .get(doc_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Watch a SUBAGENT doc (`WatchDocMessages` works for any doc id).
    /// Single-flight per key; a frozen snapshot already in place wins — the
    /// watch would race the (complete) blob with a possibly-purged live doc.
    pub fn watch_subagent_doc(&mut self, doc_id: String, cx: &mut Context<Self>) {
        if self.sub_watch_tasks.contains_key(&doc_id) {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.sub_transcripts.entry(doc_id.clone()).or_default();
        let task = spawn_subagent_watch(cx, handle, doc_id.clone());
        self.sub_watch_tasks.insert(doc_id, task);
    }

    /// Tab closed: drop the watch task (cancels the engine-side watch and
    /// unpins the doc from the engine LRU) and the rows.
    pub fn unwatch_subagent_doc(&mut self, doc_id: &str) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.sub_watch_tasks.remove(doc_id);
        self.sub_transcripts.remove(doc_id);
    }

    /// Frozen-blob path: the finished subagent's uploaded transcript, no
    /// watch needed (and any in-flight watch is superseded).
    pub fn set_subagent_snapshot(&mut self, doc_id: String, entries: Vec<SessionMessageEntry>) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.sub_watch_tasks.remove(&doc_id);
        self.sub_transcripts.insert(doc_id, entries);
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
            self.transcript_revision = self.transcript_revision.wrapping_add(1);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            let previous_len = echoes.len();
            echoes.retain(|e| e.id != message_id);
            if echoes.len() != previous_len {
                self.transcript_revision = self.transcript_revision.wrapping_add(1);
            }
        }
    }

    /// Composer send fired: overlay the chat as Working until the host writes
    /// the user message back into the transcript (or the TTL lapses). A remote
    /// send has no live session row until the host drains the queued command —
    /// that gap read as "no live run" and flashed the Completed dot, and any
    /// phantom Working→Idle edge in it rang the done-chime on send (user
    /// report 2026-08-05).
    pub fn begin_pending_send(&mut self, chat_id: &str, message_id: &str, now: DateTime<Utc>) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started: now,
            },
        );
    }

    /// Send failed — drop the overlay so the dot tells the truth again. Only
    /// removes the overlay this message started: a quick resend must not lose
    /// its own overlay to the first send's failure cleanup.
    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|p| p.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Attachment upload starting: expose its progress to the working label.
    pub fn begin_upload_progress(
        &mut self,
        total: u64,
        done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.upload_progress = Some(UploadProgress { done, total });
    }

    /// Upload leg over (success or failure) — the label goes back to plain
    /// send/working wording.
    pub fn end_upload_progress(&mut self) {
        self.upload_progress = None;
    }

    /// Percent of the in-flight attachment upload, clamped to 99 — the last
    /// point belongs to the commit + queue, so "100% but still spinning"
    /// never shows. `None` when no upload is in flight (or it's empty).
    pub fn upload_progress_percent(&self) -> Option<u8> {
        let progress = self.upload_progress.as_ref()?;
        if progress.total == 0 {
            return None;
        }
        let done = progress
            .done
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(progress.total);
        Some(((done * 100) / progress.total).min(99) as u8)
    }

    /// A `WatchTransfers` snapshot: the engine-side relay leg's in-flight
    /// queued-attachment transfers, replacing the whole set each frame.
    pub fn apply_transfers(&mut self, transfers: Vec<zeron_proto::TransferProgress>) {
        self.transfers = transfers
            .into_iter()
            .map(|t| (t.upload_id, (t.done, t.total)))
            .collect();
    }

    /// Percent of one queued attachment's relay transfer, by the uploadId
    /// its `pending://{uploadId}/…` ref names. Same 99-clamp as
    /// [`Self::upload_progress_percent`]: the last point belongs to the
    /// commit, so "100% but still spinning" never shows. `None` when no
    /// bytes are moving for that upload (staged-but-waiting, retry backoff,
    /// or done) — the thumbnail falls back to its indeterminate spinner.
    pub fn transfer_percent(&self, upload_id: &str) -> Option<u8> {
        let (done, total) = self.transfers.get(upload_id)?;
        if *total == 0 {
            return None;
        }
        Some(((done.min(total) * 100) / total).min(99) as u8)
    }

    /// Is a send still in flight for this chat (unacked)? Inside the grace
    /// window normally; while the chat's delivery path is degraded the
    /// overlay holds indefinitely — the truth IS "Queued", and silently
    /// expiring back to Idle left a queued send with no visible trace at
    /// all (the 30s→silence hole, 2026-08-19).
    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() <= UNDELIVERED_GRACE_MS
                || self.chat_delivery_degraded(chat_id)
        })
    }

    /// The send has sat unadopted past the grace window: surface the
    /// EXPLICIT failed state ("Not delivered — retry") instead of either
    /// faking progress or silently forgetting the send ever happened.
    pub fn send_undelivered(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() > UNDELIVERED_GRACE_MS
        })
    }

    /// Retry pressed: restart the grace clock so the overlay returns to its
    /// Sending/Queued phase while the re-kicked delivery runs.
    pub fn retry_pending_send(&mut self, chat_id: &str, now: DateTime<Utc>) {
        if let Some(p) = self.pending_sends.get_mut(chat_id) {
            p.started = now;
        }
    }

    /// When the in-flight send (if any, inside the TTL) was fired — the
    /// elapsed-timer base while the overlay reads as Working. The session
    /// row's `started_at` still belongs to the PREVIOUS turn during this
    /// window, and showing it made a fresh send open at the old turn's
    /// half-hour mark.
    pub fn pending_send_started(&self, chat_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.pending_sends
            .get(chat_id)
            .filter(|p| {
                now.signed_duration_since(p.started).num_milliseconds() <= UNDELIVERED_GRACE_MS
                    || self.chat_delivery_degraded(chat_id)
            })
            .map(|p| p.started)
    }

    /// The host executed the queued command iff the sent message's id showed
    /// up in the transcript (it writes the message before — causally with —
    /// the Working status; sessions.rs dispatch paths).
    fn ack_pending_send_from_transcript(&mut self) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(pending) = self.pending_sends.get(chat_id)
            && self.transcript.iter().any(|e| e.id == pending.message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        if self.no_project {
            return None;
        }
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    /// The device the new-session canvas targets: the picked project's host
    /// when one is selected, else the explicit device pick, else this device.
    pub fn effective_device_id(&self) -> Option<String> {
        if let Some(space) = self.selected_space_row() {
            return Some(space.device_id.clone());
        }
        self.selected_device
            .clone()
            .or_else(|| self.local_device_id.clone())
    }

    /// Pick the composer's target device. Keeps the project pick consistent:
    /// a project on another device can't survive the switch — fall back to
    /// the first project on the new device, else "no project".
    pub fn select_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let project_moves = self
            .selected_space_row()
            .is_some_and(|s| s.device_id != device_id);
        if project_moves {
            let first = self
                .spaces_sorted()
                .iter()
                .find(|s| s.device_id == device_id)
                .map(|s| s.id.clone());
            self.no_project = first.is_none();
            if first.is_some() {
                self.selected_space = first;
            }
        }
        self.selected_device = Some(device_id);
        cx.notify();
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// Spaces in display order — case-insensitive alphabetical, the order
    /// both space selectors (sidebar filter, composer picker) list rows in.
    /// Ties break on id so the order is stable across renders.
    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// The "@ device" tag for a space — shared by the space pickers' rows,
    /// the sidebar filter trigger, and the composer's space chip. Returns
    /// `(tag, offline)`; staleness renders as a disconnected GLYPH at the
    /// call sites (user request), never words in the tag.
    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline = !self.device_online(&space.device_id, now);
        let device = self
            .device_name(&space.device_id)
            .unwrap_or("Unknown device");
        (format!("@ {device}"), offline)
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list). A send in
    /// flight ([`Self::begin_pending_send`]) reads as Working — the queued
    /// command is as good as running.
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.send_pending(&chat.id, now) {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE space,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| match c.space_id.as_deref() {
                // Project-less sessions are first-class rows.
                None => true,
                Some(id) => self.space_row(id).is_some(),
            })
            .map(|c| (self.display_status_for(c, now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    /// The sidebar's active list exactly as it is drawn: [`Self::overview_chats`]
    /// narrowed to the current project filter. The jump shortcuts and their
    /// hints both count positions here, so neither can drift from the rows on
    /// screen.
    pub fn sidebar_chats(
        &self,
        now: DateTime<Utc>,
        space_filter: Option<&str>,
    ) -> Vec<(ChatIndicator, &Chat)> {
        self.overview_chats(now)
            .into_iter()
            .filter(|(_, chat)| match space_filter {
                Some(space_id) => chat.space_id.as_deref() == Some(space_id),
                None => true,
            })
            .collect()
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row. A send in flight reads as
    /// Working (see [`Self::display_status_for`]).
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        if self.send_pending(chat_id, now) {
            return Indicator::Working;
        }
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    /// The chat the Archive session shortcut acts on: the selected one, unless
    /// it is already archived. The shortcut archives and never unarchives, so
    /// an archived chat is left alone. Pure.
    pub fn archivable_selected_chat(&self) -> Option<&str> {
        self.selected_chat_row()
            .filter(|chat| !chat.archived)
            .map(|chat| chat.id.as_str())
    }

    /// Latest valid PR for a chat, rechecked against device, checkout, cwd and branch.
    pub fn change_request_for_chat(&self, chat: &Chat) -> Option<&ChangeRequestSummary> {
        self.change_requests
            .change_request_for_chat(chat, &self.spaces)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection, self.workspace_scope, self.auth.as_ref())
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    /// Drop every account-scoped view and subscription after its runtime has
    /// stopped. The next bootstrap must never render rows from the previous
    /// account while the local profile is opening.
    pub fn prepare_runtime_replacement(&mut self, cx: &mut Context<Self>) {
        self.engine = None;
        self.watch_tasks.clear();
        self.transcript_task = None;
        self.change_request_tasks.clear();
        self.change_requests = ChangeRequestClientState::default();
        self.connection = ConnectionStatus::Connecting;
        self.workspace_scope = None;
        self.auth = None;
        self.devices.clear();
        self.device_presentation = None;
        self.session_presence_presentation.clear();
        self.spaces.clear();
        self.chats.clear();
        self.sessions.clear();
        self.session_presentation = None;
        self.selected_space = None;
        self.no_project = false;
        self.selected_device = None;
        self.selected_chat = None;
        self.auto_selected = false;
        self.chats_synced = false;
        self.spaces_synced = false;
        self.transcript.clear();
        self.context_usage = None;
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.transcript_replayed = false;
        self.echoes.clear();
        self.pending_sends.clear();
        self.upload_progress = None;
        self.transfers.clear();
        self.local_device_id = None;
        self.update = None;
        cx.notify();
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.workspace_scope = None;
            s.auth = None;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        let engine_info = handle.engine_info();
        self.workspace_scope = Some(engine_info.workspace_scope);
        self.local_device_id = Some(engine_info.device_id.clone());
        self.engine = Some(handle.clone());
        let mut watch_tasks = Vec::with_capacity(8);
        if let Some(task) = spawn_deferred_engine_watch(cx, handle.clone()) {
            watch_tasks.push(task);
        }
        watch_tasks.extend([
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_CONNECTIVITY,
                |state, value| {
                    state.apply_connectivity(value);
                    true
                },
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_TRANSFERS,
                |state, value| {
                    state.apply_transfers(value);
                    true
                },
            ),
            spawn_watch(cx, handle.clone(), methods::WATCH_SPACES, |state, value| {
                state.apply_spaces(value);
                true
            }),
            // Auth frames parse tolerantly — engine and proto tags differ today.
            spawn_watch(cx, handle.clone(), methods::AUTH_STATUS, |state, value| {
                state.apply_auth_value(value);
                true
            }),
            spawn_watch(
                cx,
                handle.clone(),
                methods::UPDATE_STATUS,
                |state, value| {
                    state.apply_update(value);
                    true
                },
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ]);
        self.watch_tasks = watch_tasks;
        self.reconcile_change_request_watches(cx);
        // EngineInfo is part of the attachment boundary: views must know which
        // data profile they reached before they are allowed to render Ready.
        self.connection = ConnectionStatus::Ready;
        // Re-subscribe the transcript if a chat was already selected (reconnect path).
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    fn reconcile_change_request_watches(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.engine.clone() else {
            self.change_request_tasks.clear();
            return;
        };
        let targets = if self.change_requests_visible {
            desired_watch_targets(&self.chats, &self.spaces, |device| {
                !self.change_requests.is_supported(device)
            })
        } else {
            HashSet::new()
        };

        self.change_request_tasks
            .retain(|target, _| targets.contains(target));
        self.change_requests.retain_targets(&targets);

        let local_device_id = self.local_device_id.clone();
        for target in targets {
            if self.change_request_tasks.contains_key(&target) {
                continue;
            }
            let task = spawn_change_request_watch(
                cx,
                handle.clone(),
                target.clone(),
                local_device_id.clone(),
            );
            self.change_request_tasks.insert(target, task);
        }
    }

    pub fn set_change_requests_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.change_requests_visible != visible {
            self.change_requests_visible = visible;
            self.reconcile_change_request_watches(cx);
        }
    }

    pub fn open_deep_link(&mut self, url: &str, cx: &mut Context<Self>) {
        match crate::links::parse_zeron_conversation_link(url) {
            Ok(link) => {
                self.pending_deep_link = Some(link);
                self.apply_pending_deep_link(cx);
            }
            Err(error) => self.deep_link_notice = Some(error.to_string()),
        }
        cx.notify();
    }

    fn apply_pending_deep_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.pending_deep_link.clone() else {
            return;
        };
        let Some(locator) = crate::links::workspace_locator(
            self.workspace_scope,
            self.auth.as_ref(),
            self.local_device_id.as_deref(),
        ) else {
            return;
        };
        if locator != link.workspace {
            self.pending_deep_link = None;
            self.deep_link_notice =
                Some("This conversation link belongs to another workspace".into());
            return;
        }
        if self.chats.iter().any(|chat| chat.id == link.chat_id) {
            self.pending_deep_link = None;
            self.select_chat(Some(link.chat_id), cx);
        } else if self.chats_synced {
            self.pending_deep_link = None;
            self.deep_link_notice = Some("The linked conversation was not found".into());
        }
    }

    pub fn take_deep_link_notice(&mut self) -> Option<String> {
        self.deep_link_notice.take()
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.context_usage = None;
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.transcript_replayed = false;
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its project (or the lack of one); `select_chat(None)`
            // (the new-session canvas) keeps the current project pick.
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                match chat.space_id.clone() {
                    Some(space_id) => {
                        self.selected_space = Some(space_id);
                        self.no_project = false;
                    }
                    None => {
                        self.no_project = true;
                        self.selected_device = Some(chat.device_id.clone());
                    }
                }
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone()) {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    /// Select a project; the caller (shell) decides which chat to land on.
    /// `Some` clears a "Don't work in a project" opt-out and re-aims the
    /// device pick at the project's host; `None` IS that opt-out.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        match &space_id {
            Some(id) => {
                self.no_project = false;
                if let Some(device) = self.space_row(id).map(|s| s.device_id.clone()) {
                    self.selected_device = Some(device);
                }
            }
            None => self.no_project = true,
        }
        if self.selected_space == space_id && space_id.is_some() {
            cx.notify();
            return;
        }
        if space_id.is_some() {
            self.selected_space = space_id;
        }
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    /// Window-focus liveness sweep: ask the engine to probe every open room
    /// (workspace + chat docs). Fire-and-forget; each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so spamming is harmless.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.engine.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({});
            if let Err(err) = handle.client().call(methods::PROBE_SYNC, params).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }
}

/// Observe assembly after an early attach (cloud onboarding or another viewport
/// reaching the embedded engine over IPC). Data subscriptions wait on the same
/// result, but their individual errors are not authoritative: older engines may
/// legitimately omit a watch method. Only the assembly result may fail the
/// whole connection.
fn spawn_deferred_engine_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
) -> Option<Task<()>> {
    let mut deferred = handle.deferred_state()?;
    Some(cx.spawn(async move |this, cx| {
        let Err(failure) = wait_for_deferred_engine(&mut deferred).await else {
            return;
        };
        tracing::error!(error = %failure, "engine assembly failed after attachment");
        // Embedded handles release their IPC listener before exposing Retry;
        // remote handles stop their completed readiness probe.
        handle.shutdown().await;
        this.update(cx, |state, cx| {
            state.connection = ConnectionStatus::Failed(failure);
            cx.notify();
        })
        .ok();
    }))
}

/// Chats watch. Boot selection is the shell's job (it lands on the first
/// restored open tab, device-local state this entity can't see); this task
/// only pumps frames.
fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop (same contract as the transcript watch): a daemon
        // restart or RPC drop ends the stream, and a bare return here froze
        // the sidebar until app restart — new chats, renames and archives
        // from every device silently stopped arriving.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(error = %err, "chats watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: Vec<Chat> = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping malformed chats frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    state.apply_chats(parsed);
                    state.apply_pending_deep_link(cx);
                    state.reconcile_change_request_watches(cx);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!("chats stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

pub use zeron_proto::version_triple;

fn spawn_change_request_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    target: ChangeRequestWatchKey,
    local_device_id: Option<String>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let params = watch_params(&target, local_device_id.as_deref());

            let mut subscription = match handle
                .client()
                .subscribe_checked(methods::WATCH_CHECKOUT_CHANGE_REQUEST, params)
                .await
            {
                Ok(subscription) => subscription,
                Err(RpcError::UnknownMethod(_)) => {
                    tracing::debug!(
                        device = %target.device_id,
                        "checkout change requests unsupported on device"
                    );
                    this.update(cx, |state, cx| {
                        let engine_version = state
                            .devices
                            .iter()
                            .find(|device| device.id == target.device_id)
                            .and_then(|device| device.version.clone());
                        state
                            .change_requests
                            .mark_unsupported(target.device_id.clone(), engine_version);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(err) => {
                    tracing::debug!(
                        device = %target.device_id,
                        cwd = %target.cwd,
                        error = %err,
                        "checkout change request watch unavailable; retrying"
                    );
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };

            while let Some(value) = subscription.recv().await {
                let snapshot: CheckoutChangeRequestStatus = match serde_json::from_value(value) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        tracing::warn!(
                            device = %target.device_id,
                            cwd = %target.cwd,
                            error = %err,
                            "dropping malformed checkout change request frame"
                        );
                        continue;
                    }
                };
                if this
                    .update(cx, |state, cx| {
                        state.change_requests.store(target.clone(), snapshot);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }

            // Preserve the latest successful snapshot during a transport gap.
            tracing::debug!(
                device = %target.device_id,
                cwd = %target.cwd,
                "checkout change request stream ended; resubscribing"
            );
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T) -> bool,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Resubscribe loop: these are the standing Sessions/Devices/Spaces
        // watches — a daemon restart ended the stream and a bare return froze
        // them for the rest of the app's life (remote Working dots staled out
        // to nothing after 45s, and Idle/Completed transitions from other
        // devices never arrived again — "the session never completes").
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match handle
                .client()
                .subscribe(method, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(method, error = %err, "watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: T = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(method, error = %err, "dropping malformed watch frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    let changed = apply(state, parsed);
                    state.apply_pending_deep_link(cx);
                    if changed {
                        if matches!(method, methods::WATCH_SPACES | methods::WATCH_DEVICES) {
                            state.reconcile_change_request_watches(cx);
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!(method, "watch stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.local_device_id = Some(id);
                state.apply_pending_deep_link(cx);
                // Watches opened before this probe conservatively route through
                // targetDeviceId. Recreate them now that local routing is known.
                state.change_request_tasks.clear();
                state.reconcile_change_request_watches(cx);
                cx.notify();
            })
            .ok();
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop — a return here freezes the transcript
        // with no banner and no heal short of an app restart (this watch and
        // its engine-side room are the ONLY transcript delivery path). The
        // task itself is dropped by select_chat/apply_chats when the chat is
        // deselected or deleted, so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let update: zeron_doc::TranscriptUpdate = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let zeron_doc::TranscriptUpdate {
                    frame,
                    context_usage: usage,
                } = update;
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if state.context_usage != usage {
                            state.context_usage = usage;
                            cx.notify();
                        }
                        if let Err(err) = state.receive_transcript_frame(frame, cx) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// [`spawn_transcript_watch`]'s shape, writing into `sub_transcripts[doc_id]`
/// instead of the selected chat's transcript. The apply guard is PER KEY:
/// the map still holding the key (unwatch/snapshot both remove it), never
/// `selected_chat` — a subagent tab outlives chat switches.
fn spawn_subagent_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    doc_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": doc_id });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%doc_id, error = %err, "subagent watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(error = %err, "malformed subagent frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // A stale pump racing a snapshot/unwatch finds no key.
                    if let Some(rows) = state.sub_transcripts.get_mut(&doc_id) {
                        let text_only = is_text_append(&frame);
                        state.transcript_revision = state.transcript_revision.wrapping_add(1);
                        if let Err(err) = zeron_doc::apply_transcript_frame(rows, frame) {
                            tracing::warn!(%doc_id, error = %err, "resubscribing subagent watch");
                            desync = true;
                        }
                        if text_only && !desync {
                            cx.emit(TranscriptTextChanged {
                                doc_id: doc_id.clone(),
                            });
                        } else {
                            cx.notify();
                        }
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            tracing::debug!(%doc_id, "subagent stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use zeron_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `zeron_proto::view`.
    use zeron_proto::{SessionStatus, UserProfile};

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    struct LegacyIdentityRpc;

    #[async_trait]
    impl RpcService for LegacyIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::LOCAL_DEVICE => {
                    RpcReply::value(&serde_json::json!({ "deviceId": "legacy-device" }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    struct DeferredIdentityRpc {
        engine_info: EngineInfo,
        state: tokio::sync::watch::Receiver<DeferredEngineState>,
    }

    #[async_trait]
    impl RpcService for DeferredIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
                methods::ENGINE_READY => {
                    let mut state = self.state.clone();
                    wait_for_deferred_engine(&mut state)
                        .await
                        .map_err(RpcError::Failed)?;
                    RpcReply::value(&serde_json::json!({ "ready": true }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn legacy_daemon_identity_falls_back_to_synced_scope() {
        let client = memory_client(Arc::new(LegacyIdentityRpc));

        let info = query_engine_info(&client).await.unwrap();

        assert_eq!(info.device_id, "legacy-device");
        assert_eq!(info.workspace_scope, WorkspaceScope::Synced);
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(info.workspace_scope),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::SignIn
        );
    }

    #[tokio::test]
    async fn remote_viewport_treats_legacy_daemon_as_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(zeron_rpc::serve_ws_listener(
            listener,
            Arc::new(LegacyIdentityRpc),
        ));
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("legacy daemon remains attachable");

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks readiness");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deferred_engine(&mut deferred),
        )
        .await
        .expect("legacy readiness fallback completes")
        .expect("unknown EngineReady means the old daemon is assembled");

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(matches!(
            handle
                .deferred_state()
                .expect("embedded lifecycle")
                .borrow()
                .clone(),
            DeferredEngineState::Ready
        ));
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_reports_local_assembly_failure_before_returning_a_handle() {
        let dir = tempfile::tempdir().unwrap();
        zeron_engine::EngineProfile::local(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("profiles")).unwrap();
        std::fs::write(dir.path().join("profiles/local"), b"not a directory").unwrap();
        let port = free_port().await;

        let error = match EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: HarnessId::Mock,
        })
        .await
        {
            Ok(handle) => {
                handle.shutdown().await;
                panic!("a corrupt local store must fail bootstrap")
            }
            Err(error) => error,
        };

        assert!(!format!("{error:#}").is_empty());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "failed bootstrap must release the IPC listener"
        );
    }

    #[tokio::test]
    async fn deferred_engine_failure_remains_observable_after_early_attach() {
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));

        assert_eq!(
            wait_for_deferred_engine(&mut state_rx).await,
            Err("store failed".into())
        );
    }

    #[tokio::test]
    async fn remote_viewport_observes_deferred_engine_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let server = tokio::spawn(zeron_rpc::serve_ws_listener(
            listener,
            Arc::new(DeferredIdentityRpc {
                engine_info: EngineInfo {
                    device_id: "owner-device".into(),
                    workspace_scope: WorkspaceScope::Local,
                },
                state: state_rx,
            }),
        ));

        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("second viewport attaches over IPC");
        assert!(matches!(handle.mode(), EngineMode::Remote { .. }));

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks engine readiness");
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("remote readiness probe completes"),
            Err("store failed".into())
        );

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn concurrent_bootstraps_elect_one_embedded_engine() {
        // Two viewports of one app booting at once (the Local-switch restart
        // path): both used to probe a closed port, both embedded, and one lost
        // the data-dir lock. The bootstrap gate must elect exactly one owner
        // and turn the other into a plain remote attach.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let config = EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        };
        let (a, b) = tokio::join!(
            EngineHandle::bootstrap(config.clone()),
            EngineHandle::bootstrap(config.clone()),
        );
        let a = a.expect("first viewport boots");
        let b = b.expect("second viewport boots");

        let modes = [a.mode(), b.mode()];
        assert_eq!(
            modes
                .iter()
                .filter(|mode| **mode == EngineMode::InProcess)
                .count(),
            1,
            "exactly one viewport embeds: {modes:?}"
        );
        assert_eq!(
            modes
                .iter()
                .filter(|mode| matches!(mode, EngineMode::Remote { .. }))
                .count(),
            1,
            "the other attaches over IPC: {modes:?}"
        );

        for handle in [&a, &b] {
            let mut deferred = handle.deferred_state().expect("lifecycle tracked");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("readiness resolves")
            .expect("both viewports reach Ready");
        }

        b.shutdown().await;
        a.shutdown().await;
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn production_bootstrap_opens_local_data_without_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        assert_eq!(handle.engine_info().workspace_scope, WorkspaceScope::Local);
        let info: EngineInfo = handle
            .client()
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(info, *handle.engine_info());

        let mut auth = handle
            .client()
            .subscribe(methods::AUTH_STATUS, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            parse_auth_state(&auth.recv().await.unwrap()),
            Some(AuthState::SignedOut)
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .expect("local data RPC is immediately available");
        assert!(harnesses.as_array().is_some_and(|items| !items.is_empty()));
        assert!(
            !dir.path().join("orgs/dev-org/dev-user").exists(),
            "production boot must not create dev-user data"
        );
        assert!(dir.path().join("profiles/local").is_dir());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn engine_info_is_available_while_cloud_onboarding_is_deferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session.json"),
            r#"{"refreshToken":"saved","user":{"id":"user_1","email":"u@example.com"}}"#,
        )
        .unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        assert!(matches!(
            handle
                .deferred_state()
                .expect("embedded lifecycle")
                .borrow()
                .clone(),
            DeferredEngineState::Waiting
        ));

        let info: EngineInfo = handle
            .client()
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await
            .expect("EngineInfo bypasses deferred cloud stores");
        assert_eq!(info.workspace_scope, WorkspaceScope::Synced);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                handle
                    .client()
                    .call(methods::LIST_HARNESSES, serde_json::json!({})),
            )
            .await
            .is_err(),
            "cloud data waits for organization onboarding"
        );
        assert!(!dir.path().join("orgs").exists());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `zeron headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(zeron_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        assert_eq!(
            handle.engine_info().workspace_scope,
            WorkspaceScope::Development
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        assert!(matches!(
            handle
                .client()
                .call(methods::STOP_ENGINE, serde_json::json!({}))
                .await,
            Err(RpcError::UnknownMethod(method)) if method == methods::STOP_ENGINE
        ));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    fn user_entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: zeron_doc::MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn transcript_revision_tracks_replay_echoes_and_subagent_content() {
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        let initial = state.transcript_revision;
        state.apply_transfers(Vec::new());
        state.apply_auth(AuthState::SignedOut);
        assert_eq!(
            state.transcript_revision, initial,
            "unrelated notifications are inert"
        );

        state.push_echo("c", user_entry("m1"));
        let echo = state.transcript_revision;
        assert_ne!(echo, initial);
        state.push_echo("c", user_entry("m1"));
        state.remove_echo("c", "absent");
        assert_eq!(
            state.transcript_revision, echo,
            "unchanged echoes do not invalidate"
        );
        state.apply_transcript(vec![user_entry("m1")]);
        assert!(state.pending_echoes().is_empty());
        assert_ne!(
            state.transcript_revision, echo,
            "echo-to-replay handoff invalidates"
        );

        let before_reset = state.transcript_revision;
        state
            .apply_transcript_frame(TranscriptFrame::Reset { reset: Vec::new() })
            .unwrap();
        assert!(state.transcript_replayed);
        assert_ne!(
            state.transcript_revision, before_reset,
            "empty replay is authoritative"
        );

        let before_subagent = state.transcript_revision;
        state.set_subagent_snapshot("sub".into(), vec![user_entry("nested")]);
        assert_ne!(state.transcript_revision, before_subagent);
        let before_close = state.transcript_revision;
        state.unwatch_subagent_doc("sub");
        assert!(state.sub_transcript("sub").is_empty());
        assert_ne!(state.transcript_revision, before_close);
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn session_heartbeats_preserve_freshness_without_redrawing_unchanged_status() {
        let mut state = AppState::new();
        let now = Utc::now();
        let mut row = Session {
            chat_id: "chat".into(),
            device_id: "host".into(),
            status: SessionStatus::Working,
            started_at: Some(now),
            updated_at: now,
        };
        assert!(state.apply_sessions_at(vec![row.clone()], now));
        row.updated_at = now + TimeDelta::seconds(30);
        assert!(!state.apply_sessions_at(vec![row.clone()], now + TimeDelta::seconds(30)));
        assert_eq!(state.sessions[0].updated_at, row.updated_at);
        assert_eq!(
            state.indicator_for("chat", now + TimeDelta::seconds(60)),
            Indicator::Working
        );
        assert!(state.apply_sessions_at(vec![row.clone()], now + TimeDelta::seconds(76)));
        row.updated_at = now + TimeDelta::seconds(80);
        assert!(state.apply_sessions_at(vec![row.clone()], now + TimeDelta::seconds(80)));
        row.status = SessionStatus::AwaitingInput;
        assert!(state.apply_sessions_at(vec![row.clone()], now + TimeDelta::seconds(80)));
        row.status = SessionStatus::Idle;
        row.started_at = None;
        assert!(state.apply_sessions_at(vec![row], now + TimeDelta::seconds(80)));
        assert!(state.apply_sessions_at(vec![], now + TimeDelta::seconds(80)));
    }

    #[test]
    fn unchanged_device_heartbeat_still_retires_stale_session_indicators() {
        let mut state = AppState::new();
        let now = Utc::now();
        state.sessions = vec![Session {
            chat_id: "chat".into(),
            device_id: "host".into(),
            status: SessionStatus::Working,
            started_at: Some(now),
            updated_at: now,
        }];
        let mut row = device("host", "Host");
        row.last_seen_at = Some(now);
        assert!(state.apply_devices_at(vec![row.clone()], now));
        row.last_seen_at = Some(now + TimeDelta::seconds(30));
        assert!(!state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(30)));
        row.last_seen_at = Some(now + TimeDelta::seconds(46));
        assert!(state.apply_devices_at(vec![row], now + TimeDelta::seconds(46)));
        assert_eq!(
            state.indicator_for("chat", now + TimeDelta::seconds(46)),
            Indicator::None
        );
        let mut recovered = state.sessions.clone();
        recovered[0].updated_at = now + TimeDelta::seconds(47);
        assert!(
            state.apply_sessions_at(recovered, now + TimeDelta::seconds(47)),
            "a heartbeat must immediately restore an indicator retired by a device tick"
        );
    }

    #[test]
    fn device_heartbeats_keep_freshness_without_repainting_unchanged_presentation() {
        let mut state = AppState::new();
        let now = Utc::now();
        let mut row = device("host", "Host");
        row.last_seen_at = Some(now);
        assert!(state.apply_devices_at(vec![row.clone()], now));
        row.last_seen_at = Some(now + TimeDelta::seconds(15));
        assert!(!state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(15)));
        assert_eq!(state.devices[0].last_seen_at, row.last_seen_at);
        // The settings label still advances even before the presence dot expires.
        assert!(state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(75)));
        // An identical frame must redraw when presence crosses the expiry boundary.
        assert!(state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(86)));
        assert!(!state.device_online("host", now + TimeDelta::seconds(86)));
        row.last_seen_at = Some(now + TimeDelta::seconds(90));
        assert!(state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(90)));
        assert!(state.device_online("host", now + TimeDelta::seconds(90)));
        row.name = "Renamed".into();
        assert!(state.apply_devices_at(vec![row.clone()], now + TimeDelta::seconds(90)));
        row.version = Some("9.0.0".into());
        assert!(state.apply_devices_at(vec![row], now + TimeDelta::seconds(90)));
        assert!(state.apply_devices_at(vec![], now + TimeDelta::seconds(90)));
        assert!(!state.apply_devices_at(vec![], now + TimeDelta::seconds(90)));
    }

    #[test]
    fn local_workspace_hides_the_unknown_device_sentinel() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.local_device_id = Some("local".into());

        state.apply_devices(vec![
            device("local", "unknown-device"),
            device("remote", "unknown-device"),
        ]);

        assert_eq!(state.device_name("local"), Some("Local"));
        assert_eq!(state.device_name("remote"), Some("unknown-device"));

        state.apply_devices(vec![device("local", "José's MacBook Pro")]);
        assert_eq!(state.device_name("local"), Some("José's MacBook Pro"));
    }

    #[test]
    fn device_version_change_reenables_change_request_capability() {
        let mut state = AppState::new();
        state
            .change_requests
            .mark_unsupported("remote".into(), Some("0.2.2".into()));
        let mut old = device("remote", "Remote");
        old.version = Some("0.2.2".into());
        state.apply_devices(vec![old]);
        assert!(!state.change_requests.is_supported("remote"));

        let mut upgraded = device("remote", "Remote");
        upgraded.version = Some("0.2.3".into());
        state.apply_devices(vec![upgraded]);
        assert!(state.change_requests.is_supported("remote"));
    }

    #[test]
    fn transfer_percent_tracks_snapshots_by_upload_id() {
        let mut s = AppState::new();
        assert_eq!(s.transfer_percent("u1"), None);

        let frame = |id: &str, done, total| zeron_proto::TransferProgress {
            upload_id: id.into(),
            file_name: "a.png".into(),
            done,
            total,
        };
        s.apply_transfers(vec![frame("u1", 430, 1_000), frame("u2", 0, 400)]);
        assert_eq!(s.transfer_percent("u1"), Some(43));
        assert_eq!(s.transfer_percent("u2"), Some(0));
        assert_eq!(s.transfer_percent("other"), None);

        // The last point belongs to the commit — never a stuck 100%; b64
        // padding overshoot stays clamped too.
        s.apply_transfers(vec![frame("u1", 1_000, 1_000), frame("u3", 12, 0)]);
        assert_eq!(s.transfer_percent("u1"), Some(99));
        // Zero-total renders indeterminate, not a division blowup.
        assert_eq!(s.transfer_percent("u3"), None);
        // Snapshots REPLACE: u2's transfer retired with its entry.
        assert_eq!(s.transfer_percent("u2"), None);

        s.apply_transfers(Vec::new());
        assert_eq!(s.transfer_percent("u1"), None);
    }

    #[test]
    fn upload_progress_percent_clamps_and_clears() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let mut s = AppState::new();
        assert_eq!(s.upload_progress_percent(), None);

        let done = Arc::new(AtomicU64::new(0));
        s.begin_upload_progress(1_000, done.clone());
        assert_eq!(s.upload_progress_percent(), Some(0));
        done.store(430, Ordering::Relaxed);
        assert_eq!(s.upload_progress_percent(), Some(43));
        // The last point belongs to commit+queue — never show a stuck 100%.
        done.store(1_000, Ordering::Relaxed);
        assert_eq!(s.upload_progress_percent(), Some(99));
        // Overshoot (b64 padding rounding) stays clamped.
        done.store(1_002, Ordering::Relaxed);
        assert_eq!(s.upload_progress_percent(), Some(99));

        s.end_upload_progress();
        assert_eq!(s.upload_progress_percent(), None);

        // A zero-byte total renders as plain "Sending…", not a percent.
        s.begin_upload_progress(0, Arc::new(AtomicU64::new(0)));
        assert_eq!(s.upload_progress_percent(), None);
    }

    #[test]
    fn send_pending_overlays_working_until_the_grace_window() {
        let now = Utc::now();
        let s_chat = chat("c", 0, Some(10)); // unseen, no session row
        let mut s = AppState::new();
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Completed);
        assert_eq!(s.indicator_for("c", now), Indicator::None);
        s.begin_pending_send("c", "m1", now);
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Working);
        assert_eq!(s.indicator_for("c", now), Indicator::Working);
        // Time-bounded: an offline host must not leave an eternal spinner —
        // past the grace the overlay yields (and `send_undelivered` takes
        // over with the explicit failed state).
        let later = now + TimeDelta::milliseconds(UNDELIVERED_GRACE_MS + 1);
        assert_eq!(
            s.display_status_for(&s_chat, later),
            ChatIndicator::Completed
        );
        assert_eq!(s.indicator_for("c", later), Indicator::None);
        assert!(s.send_undelivered("c", later));
    }

    #[test]
    fn send_pending_acked_when_the_host_writes_the_message_back() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.selected_chat = Some("c".into());
        s.begin_pending_send("c", "m1", now);
        // A frame without the message keeps the overlay.
        s.apply_transcript(vec![user_entry("other")]);
        assert!(s.send_pending("c", now));
        // The host executed the command: our id comes back in the doc.
        s.apply_transcript(vec![user_entry("other"), user_entry("m1")]);
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn send_failure_cleanup_only_ends_its_own_overlay() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.begin_pending_send("c", "m1", now);
        s.begin_pending_send("c", "m2", now); // quick resend superseded m1
        s.end_pending_send("c", "m1"); // m1's failure cleanup arrives late
        assert!(s.send_pending("c", now), "m2's overlay must survive");
        s.end_pending_send("c", "m2");
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-space chat (idle included) PLUS
        // project-less chats (first-class since the project selectors);
        // chats of unknown spaces stay hidden. Completed ("old") outranks
        // idle ("new"/"dangling").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new", "dangling"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = zeron_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(zeron_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            zeron_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn jump_slots_count_the_rows_the_sidebar_draws() {
        let now = Utc::now();
        let mut state = AppState::new();
        let mut in_space = chat("a", 0, Some(3));
        in_space.space_id = Some("s1".into());
        let mut other_space = chat("b", 1, Some(2));
        other_space.space_id = Some("s2".into());
        let mut archived = chat("gone", 2, Some(1));
        archived.space_id = Some("s1".into());
        archived.archived = true;
        state.apply_spaces(vec![
            space("s1", "d1", "/tmp/s1", 0),
            space("s2", "d1", "/tmp/s2", 1),
        ]);
        state.apply_chats(vec![in_space, other_space, archived]);

        // The archived row is not in the active list, so no slot reaches it.
        let order: Vec<&str> = state
            .sidebar_chats(now, None)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(order.len(), 2);
        assert!(!order.contains(&"gone"));

        // A project filter renumbers: the visible rows only.
        let filtered: Vec<&str> = state
            .sidebar_chats(now, Some("s2"))
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(filtered, ["b"]);
    }

    #[test]
    fn archive_shortcut_only_targets_an_open_active_chat() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, None);
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        // No chat open: nothing to archive.
        assert_eq!(state.archivable_selected_chat(), None);
        // The open active chat is the target.
        state.selected_chat = Some("b".into());
        assert_eq!(state.archivable_selected_chat(), Some("b"));
        // An already archived chat stays put — the shortcut never unarchives.
        state.selected_chat = Some("a".into());
        assert_eq!(state.archivable_selected_chat(), None);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: zeron_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn transcript_replay_barrier_requires_the_opening_reset() {
        let mut state = AppState::new();
        assert!(!state.transcript_replayed);

        state
            .apply_transcript_frame(TranscriptFrame::Delta {
                upsert: Vec::new(),
                append: Vec::new(),
                remove: Vec::new(),
                count: 0,
            })
            .expect("empty delta");
        assert!(!state.transcript_replayed);

        state
            .apply_transcript_frame(TranscriptFrame::Reset { reset: Vec::new() })
            .expect("empty reset");
        assert!(state.transcript_replayed);
    }

    #[test]
    fn gate_phases() {
        let user = UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        };
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into()), None, None),
            GatePhase::Failed("boom".into())
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Local),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::SignIn
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedIn {
                    user: user.clone(),
                    org_id: None
                })
            ),
            GatePhase::Ready
        );
        // No org yet → org gate.
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::NeedsOrganization { user })
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn auth_changes_do_not_change_a_local_runtime_scope_or_watches() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.watch_tasks.push(Task::ready(()));

        state.apply_auth(AuthState::NeedsOrganization {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);

        state.apply_auth(AuthState::SignedIn {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);
    }

    #[test]
    fn auth_frames_parse_both_wire_shapes() {
        // Proto shape.
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        // Engine shape (`_tag`, PascalCase, orgId).
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "orgId": "org-1",
        });
        let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(org_id.as_deref(), Some("org-1"));
        let needs = serde_json::json!({
            "_tag": "NeedsOrganization",
            "user": { "id": "u1", "email": "w@example.com", "name": "W" },
        });
        assert!(matches!(
            parse_auth_state(&needs),
            Some(AuthState::NeedsOrganization { .. })
        ));
        // Garbage → None (frame dropped, not a crash).
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/zeron")), "zeron");
        assert_eq!(project_label(Some("/home/w/dev/zeron/")), "zeron");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/zeron")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/zeron")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["zeron", "zed", "No project"]);
        let zeron_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(zeron_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_zeron_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("zeron/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · zeron/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn org_gate_reducers() {
        assert!(org_name_valid("Acme"));
        assert!(org_name_valid("  padded  "));
        assert!(!org_name_valid(""));
        assert!(!org_name_valid("   "));
        assert!(!org_name_valid(&"x".repeat(65)));

        let rows = parse_orgs(&serde_json::json!({ "orgs": [
            { "id": "m2", "organizationId": "o2", "name": "beta" },
            { "id": "m1", "organizationId": "o1", "name": "Alpha" },
            { "id": "m3", "organizationId": "o1", "name": "Alpha" },
        ]}));
        assert_eq!(rows.len(), 3);
        let sorted = sort_memberships(rows);
        let names: Vec<&str> = sorted.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            ["Alpha", "beta"],
            "case-insensitive sort + dedupe by org id"
        );
        // Bare-array replies parse too; garbage yields empty.
        assert_eq!(
            parse_orgs(&serde_json::json!([{ "id": "m", "organizationId": "o", "name": "n" }]))
                .len(),
            1
        );
        assert!(parse_orgs(&serde_json::json!("nope")).is_empty());
    }

    #[test]
    fn review_comment_flush_waits_for_every_file_surface() {
        let mut state = AppState::new();

        state.begin_review_comment_flush("chat-1", 1);
        state.begin_review_comment_flush("chat-1", 2);
        assert!(state.review_comment_flush_pending("chat-1"));

        state.finish_review_comment_flush("chat-1", 1);
        assert!(state.review_comment_flush_pending("chat-1"));

        state.finish_review_comment_flush("chat-1", 2);
        assert!(!state.review_comment_flush_pending("chat-1"));
    }

    #[test]
    fn version_triple_parses_and_gates_device_features() {
        assert_eq!(version_triple("0.2.12"), Some((0, 2, 12)));
        assert_eq!(version_triple("0.2.12-beta.1"), Some((0, 2, 12)));
        assert_eq!(version_triple("1.0.0+build7"), Some((1, 0, 0)));
        assert_eq!(version_triple("0.2"), None);
        assert_eq!(version_triple("garbage"), None);

        let mut s = AppState::default();
        assert!(
            !s.device_version_at_least("d1", (0, 2, 12)),
            "unknown device conservatively fails the gate"
        );
        s.devices = vec![Device {
            id: "d1".into(),
            name: "laptop".into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: Some("0.2.12".into()),
        }];
        assert!(s.device_version_at_least("d1", (0, 2, 12)));
        assert!(!s.device_version_at_least("d1", (0, 2, 13)));
        s.devices[0].version = None;
        assert!(
            !s.device_version_at_least("d1", (0, 2, 12)),
            "unstamped version conservatively fails the gate"
        );
    }

    #[test]
    fn delivery_degradation_and_queued_sends_tell_the_truth() {
        use zeron_proto::{ChatConnectivity, ConnectivityState};
        let now = Utc::now();
        let mut s = AppState::default();
        s.local_device_id = Some("local".into());
        let mut remote = chat("c-remote", 0, None);
        remote.device_id = "remote".into();
        let mut local = chat("c-local", 0, None);
        local.device_id = "local".into();
        s.chats = vec![remote, local];
        s.devices = vec![Device {
            id: "remote".into(),
            name: "vps".into(),
            platform: "linux".into(),
            last_seen_at: Some(now),
            created_at: None,
            version: None,
        }];
        s.connectivity.state = ConnectivityState::Connected;
        s.connectivity.chats = vec![ChatConnectivity {
            chat_id: "c-remote".into(),
            connected: true,
            pending_pushes: 0,
        }];

        // Healthy: nothing degraded.
        assert!(!s.chat_delivery_degraded("c-remote"));
        assert!(!s.chat_delivery_degraded("c-local"));

        // The chat's own room down → degraded even while globally Connected.
        s.connectivity.chats[0].connected = false;
        assert!(s.chat_delivery_degraded("c-remote"));
        s.connectivity.chats[0].connected = true;

        // Host gone presence-dark → degraded (a send would queue at best).
        s.devices[0].last_seen_at = Some(now - TimeDelta::minutes(10));
        assert!(s.chat_delivery_degraded("c-remote"));
        s.devices[0].last_seen_at = Some(now);

        // OS offline: remote degrades; a locally-hosted chat NEVER does (the
        // queued command executes on this device even fully offline).
        s.connectivity.state = ConnectivityState::Offline;
        assert!(s.chat_delivery_degraded("c-remote"));
        assert!(!s.chat_delivery_degraded("c-local"));

        // Local profile (Disabled): nothing degrades.
        s.connectivity.state = ConnectivityState::Disabled;
        assert!(!s.chat_delivery_degraded("c-remote"));

        // Queued = pending send + degraded path — and degradation HOLDS the
        // overlay past the grace window instead of silently expiring (the
        // no-trace hole).
        s.connectivity.state = ConnectivityState::Offline;
        s.begin_pending_send("c-remote", "m1", now - TimeDelta::seconds(200));
        assert!(s.send_pending("c-remote", now));
        assert!(s.send_queued("c-remote", now));
        // Past the grace: the explicit failed state surfaces alongside.
        assert!(s.send_undelivered("c-remote", now));
        // Retry restarts the clock: back to Queued, no longer failed.
        s.retry_pending_send("c-remote", now);
        assert!(!s.send_undelivered("c-remote", now));
        assert!(s.send_queued("c-remote", now));
        // Path healed with a stale unacked send: the overlay expires after
        // the grace rather than holding forever.
        s.retry_pending_send("c-remote", now - TimeDelta::seconds(200));
        s.connectivity.state = ConnectivityState::Connected;
        assert!(!s.send_pending("c-remote", now));
        assert!(!s.send_queued("c-remote", now));
        // …but the explicit undelivered flag still tells the truth.
        assert!(s.send_undelivered("c-remote", now));
    }
}
