//! DocHost — per-chat `SessionDoc` handles: snapshot persistence (debounced), edge room
//! sync (offline-tolerant), and the HOST-ONLY durable command executor.
//!
//! Pragmatic port of zeron's `session-docs.ts` + the `main.ts` executor (spec:
//! feature-inventory §3.3, ARCHITECTURE §2 "command plane"):
//! - the doc IS the outbox: commands and user entries commit locally and sync whenever a
//!   room connection exists; the engine is fully functional with sync disabled;
//! - on every doc change (local commit or remote import) the handle re-emits the joined
//!   transcript to watchers, drains pending commands, and schedules a snapshot save;
//! - command drain: evaluate via `evaluate_command` (with the DocsStore processed
//!   ledger), mark processed BEFORE execute, execute through the sessions engine, then
//!   write the outcome status back into the doc as the sole outcome writer.
//!
//! Chat ownership is gated on the workspace doc (`chats[chat_id].deviceId`), with
//! claim-on-first-command for unknown chats. Queueing a command for a chat hosted on
//! another device POSTs a durable nudge to that device's room (§7 cold-chat delivery);
//! the host's relay receives it and warm-opens the doc, which drains the queue.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use base64::Engine as _;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use zeron_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry, evaluate_command,
    join_continuation_entries,
};
use zeron_proto::{ConversationSourceContext, HarnessId, UserInputAnswer, UserInputQuestion};
use zeron_sync::DocsStore;

use crate::sessions::{SessionsEngine, SteerOutcome};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

/// Warm-doc LRU: how many unwatched, run-less docs stay fully open. Everything
/// beyond this (and beyond [`zeron_doc::DOC_LRU_BYTE_BUDGET`]) is evicted
/// oldest-access-first — reopening from the SQLite snapshot measured within
/// ~11ms of a warm doc, so the cap trades no perceptible open latency.
const WARM_DOC_CAP: usize = 12;

/// Resident-memory estimate per compressed snapshot byte. Loro snapshots are
/// columnar+compressed; the in-memory doc plus mirror runs well above the blob
/// size. A rough multiplier is enough here — the budget is a safety ceiling,
/// the count cap does the day-to-day work.
const RESIDENT_BYTES_PER_SNAPSHOT_BYTE: usize = 6;

/// Floor per open doc (room socket buffers, tasks) regardless of content size.
const DOC_RESIDENT_FLOOR_BYTES: usize = 512 * 1024;

/// Docs touched this recently are never evicted. Closes the open→attach race:
/// `open()` returns a handle, and until the caller's `watch_messages` lands
/// the doc is unwatched and unpinned — a concurrent eviction would orphan the
/// watcher on a roomless doc that renders once and never updates again.
const EVICT_MIN_IDLE_MS: i64 = 30_000;

/// Queued-attachment transfer pacing: chunk pushes are bounded per call (a
/// stalled-but-open relay link never fails on its own) and a timeout marks
/// the link suspect; attempts retry on this backoff, cut short by the online
/// bus / system wake.
const TRANSFER_CHUNK_B64: usize = 60_000;
const TRANSFER_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const TRANSFER_COMMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const TRANSFER_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(2);
const TRANSFER_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
/// A command whose attachment bytes are still in transit waits at most this
/// long before the drain rejects it loudly (and the transfer task gives up on
/// the same clock) — a chat must never wedge behind bytes that aren't coming.
const ATTACHMENT_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const ATTACHMENT_WAIT_MAX_MS: i64 = ATTACHMENT_WAIT_MAX.as_millis() as i64;
/// Re-check cadence while a chat's queue is deferred on in-transit bytes
/// (the happy path is event-driven — UploadCommit kicks the drain — this
/// timer only covers the give-up transition and missed kicks).
const ATTACHMENT_WAIT_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Transfer attempt outcome: transient failures retry (the link may heal),
/// permanent ones stop (the host actively refused, or the bytes are gone).
enum TransferError {
    Transient(String),
    Permanent(String),
}

/// Peer-relay delivery fallback pacing (`spawn_command_delivery`): the grace
/// the normal rows→edge path gets before the relay road opens, the poll while
/// waiting, the relay retry curve, its per-call deadline, and the give-up cap
/// (the command stays durably queued in the doc regardless).
const ROWS_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
const ROWS_POLL: std::time::Duration = std::time::Duration::from_secs(1);
const RELAY_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(5);
const RELAY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
const RELAY_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const RELAY_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Hosts below this stamped engine version don't serve `RelayCommand`.
const RELAY_MIN_VERSION: (u64, u64, u64) = (0, 2, 12);

/// Edge connection config. The bearer is a **provider**, never a snapshot:
/// every room (re)connect and HTTP request re-reads it, so WorkOS access-token
/// refreshes (~1h expiry) take effect without an engine restart. Dev bearers
/// (which never expire) ride the same seam as a [`zeron_rpc::StaticToken`].
#[derive(Clone)]
pub struct EdgeConfig {
    /// Edge base URL (`http(s)://…`); rewritten to `ws(s)` for the room socket.
    pub url: String,
    /// Fresh-bearer provider (the relay's `TokenSource`), consulted per
    /// connect/request. `None` from the provider = signed out.
    pub token: Arc<dyn zeron_rpc::TokenSource>,
    /// This engine's device id, carried on room dials (`&device=`) so the
    /// edge can attribute sockets in logs. Debugging the 2026-08-04 deaf
    /// socket meant reverse-engineering devices from rotating IPv6 privacy
    /// addresses; never again. Empty = omitted (tests).
    pub device_id: String,
}

impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field("url", &self.url)
            .field("token", &"<provider>")
            .finish()
    }
}

impl EdgeConfig {
    pub fn new(url: impl Into<String>, token: Arc<dyn zeron_rpc::TokenSource>) -> Self {
        Self {
            url: url.into(),
            token,
            device_id: String::new(),
        }
    }

    /// Attribute this engine's room sockets in edge logs.
    pub fn with_device(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = device_id.into();
        self
    }

    /// Fixed bearer — dev mode and tests, where tokens never expire.
    pub fn with_static_token(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(url, Arc::new(zeron_rpc::StaticToken(token.into())))
    }

    /// The current bearer, refreshed by the provider if stale. `None` = signed out.
    pub async fn bearer(&self) -> Option<String> {
        self.token.token().await
    }

    pub fn token_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.token.subscribe()
    }

    /// A per-dial room URL provider for `path` (e.g. `/session/{chatId}/ws`):
    /// the bearer is re-fetched before every connect, so reconnects after a
    /// token expiry present a fresh `?token=` instead of the boot-time one.
    pub fn room_url(&self, path: impl Into<String>) -> Arc<dyn zeron_sync::UrlProvider> {
        let ws_base = self.url.replacen("http", "ws", 1);
        Arc::new(EdgeRoomUrl {
            base: format!("{}{}", ws_base.trim_end_matches('/'), path.into()),
            token: self.token.clone(),
            device_id: self.device_id.clone(),
        })
    }
}

struct EdgeRoomUrl {
    base: String,
    token: Arc<dyn zeron_rpc::TokenSource>,
    device_id: String,
}

impl zeron_sync::UrlProvider for EdgeRoomUrl {
    fn url(&self) -> futures::future::BoxFuture<'static, Result<String, zeron_sync::SyncError>> {
        let token = self.token.clone();
        let base = self.base.clone();
        let device = self.device_id.clone();
        Box::pin(async move {
            let token = token.token().await.ok_or_else(|| {
                zeron_sync::SyncError::Auth("no access token (signed out)".into())
            })?;
            let mut url = format!("{base}?token={token}");
            if !device.is_empty() {
                url.push_str(&format!("&device={device}"));
            }
            Ok(url)
        })
    }
}

#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// When present, each opened chat joins its edge session room. `None` = fully
    /// offline operation (local snapshots only).
    pub edge: Option<EdgeConfig>,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    /// Set-once (first wins), cleared by `shutdown_workers`: sessions and
    /// doc-host reference each other through Arcs, so a retired runtime's
    /// graph only drops once this back-edge is severed.
    sessions: Mutex<Option<SessionsEngine>>,
    workspace: OnceLock<WorkspaceHost>,
    /// Worktree materialization for Run commands (see `set_repos`).
    repos: OnceLock<crate::repos::Repos>,
    /// Cancels every worker spawned through `spawn_worker` — the loops'
    /// own exit conditions (weak handle death, closed channels) don't cover
    /// runtime replacement, where Edge-capable tasks must stop doing
    /// network work even while something still pins the graph.
    shutdown: CancellationToken,
    /// Tracks every spawned worker so `shutdown_workers` can await them.
    tasks: TaskTracker,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
    /// chat2 seeds in flight (one per chat — reopen storms must not race
    /// duplicate rebuild+checkpoint POSTs; benign server-side, wasteful).
    seeding: Mutex<HashSet<String>>,
    /// chat2 quiet-waiters armed (one per chat): the cutover watcher re-arms
    /// on every registry change, and a long run would stack a tick loop per
    /// change without this.
    seed_waiting: Mutex<HashSet<String>>,
    /// Attachment-wait re-drain timers armed (one per chat): a command
    /// deferred on in-transit attachment bytes re-checks on a cadence, and
    /// each deferral must not stack another timer.
    drain_waiting: Mutex<HashSet<String>>,
    /// Uploads store (engine assembly) — resolves `pending://` attachment
    /// refs and jails transfer reads to the uploads dir.
    uploads: OnceLock<crate::uploads::Uploads>,
    /// Connectivity watch (`WatchConnectivity`): lazily-started monitor
    /// publishes the edge posture on change (see `watch_connectivity`).
    connectivity: OnceLock<watch::Sender<zeron_proto::Connectivity>>,
    connectivity_started: AtomicBool,
    /// In-flight queued-attachment transfers, published per landed chunk
    /// (see `watch_transfers`). Entries live exactly as long as bytes are
    /// moving: added when a file's push starts, removed on commit or failure
    /// (a retry re-adds), so consumers can render a real percent while the
    /// relay leg runs and fall back to indeterminate otherwise.
    transfers: watch::Sender<Vec<zeron_proto::TransferProgress>>,
    connectivity_grace: Mutex<DegradeGrace>,
    /// Command ids currently BETWEEN mark-processed and their resolution in a
    /// drain. Distinguishes "executing right now" from "consumed by the
    /// ledger but dead" (a crash between mark and resolve): the drain
    /// terminalizes the latter as Rejected instead of leaving a forever-
    /// Pending entry no retry could ever reach (2026-08-19 swallowed-send).
    executing: Mutex<HashSet<String>>,
    /// Peer links (engine assembly, edge runtimes only) — the transport that
    /// pushes queued attachment bytes to a remote host.
    links: OnceLock<Arc<zeron_rpc::LinkCache>>,
    /// Shared client for sidecar blob PUT/GET (30s timeout, uploads.rs
    /// discipline — diff_sync's untimed client hung on dead links).
    http: reqwest::Client,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The queued-attachment transfers a command's `pending://` refs imply —
/// shared by the retry escort and the retry re-issue.
fn command_transfers(entry: &SessionCommandEntry) -> Vec<crate::uploads::AttachmentTransfer> {
    let refs: Vec<String> = match &entry.payload {
        SessionCommandPayload::Run { request, .. } => request
            .attachments
            .iter()
            .filter(|p| crate::uploads::is_pending_ref(p))
            .cloned()
            .collect(),
        SessionCommandPayload::Steer { prompt, .. } => crate::uploads::pending_refs_in(prompt),
        _ => Vec::new(),
    };
    refs.iter()
        .filter_map(|r| crate::uploads::parse_pending_ref(r))
        .map(
            |(upload_id, file_name)| crate::uploads::AttachmentTransfer {
                upload_id: upload_id.to_string(),
                file_name: file_name.to_string(),
            },
        )
        .collect()
}

/// Retires a transfer's progress entry on drop — the one exit point for
/// `push_attachments`' many returns (commit landed, chunk timeout, host
/// refusal), so no failure path can leave a phantom ring behind.
struct TransferProgressGuard<'a> {
    host: &'a DocHost,
    upload_id: &'a str,
}

impl Drop for TransferProgressGuard<'_> {
    fn drop(&mut self) {
        self.host.transfer_progress_clear(self.upload_id);
    }
}

/// How long raw degradation must persist before connectivity reports it.
/// Room joins, idle-link wakes, and navigation dials resolve well under a
/// second on healthy networks; real outages outlive this comfortably. Recovery
/// is never delayed.
const DEGRADE_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// One tracked degradation source in [`DegradeGrace`].
enum GraceKey<'a> {
    OsPath,
    Registry,
    Chat(&'a str),
}

/// Show-slow / hide-fast hysteresis over raw connectivity signals. Pure over
/// injected `Instant`s so the grace window is unit-testable.
#[derive(Default)]
struct DegradeGrace {
    os_path: Option<std::time::Instant>,
    registry: Option<std::time::Instant>,
    chats: HashMap<String, std::time::Instant>,
}

impl DegradeGrace {
    /// Feed one raw sample; returns whether to REPORT the source as degraded.
    /// Healthy clears the timer instantly; degraded reports only once it has
    /// persisted for [`DEGRADE_GRACE`].
    fn degraded(&mut self, key: GraceKey, raw: bool, now: std::time::Instant) -> bool {
        let slot: &mut Option<std::time::Instant> = match key {
            GraceKey::OsPath => &mut self.os_path,
            GraceKey::Registry => &mut self.registry,
            GraceKey::Chat(id) => {
                if raw && !self.chats.contains_key(id) {
                    self.chats.insert(id.to_string(), now);
                }
                match self.chats.get_mut(id) {
                    Some(_) if !raw => {
                        self.chats.remove(id);
                        return false;
                    }
                    Some(since) => return now.duration_since(*since) >= DEGRADE_GRACE,
                    None => return false,
                }
            }
        };
        if !raw {
            *slot = None;
            return false;
        }
        let since = *slot.get_or_insert(now);
        now.duration_since(since) >= DEGRADE_GRACE
    }

    /// Drop timers for chats that no longer have open docs.
    fn retain_chats(&mut self, keep: impl Fn(&str) -> bool) {
        self.chats.retain(|id, _| keep(id));
    }
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

/// One open chat doc: the `SessionDoc`, its change plumbing, and the room client.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<Arc<Vec<SessionMessageEntry>>>,
    /// True when the doc changed while nobody watched: the mirror rebuild is
    /// deferred to the next `watch_messages` attach instead of paid per commit.
    mirror_dirty: AtomicBool,
    /// Epoch ms of the last open/watch touch — the LRU eviction key.
    last_access: AtomicI64,
    /// Last known snapshot blob size — the eviction budget estimate's input.
    snapshot_bytes: AtomicUsize,
    /// The sync generation this handle was BUILT for (1 = legacy s2,
    /// 2 = chat2). Gen-1 handles no longer join any room (the s2 client is
    /// gone); they serve the local fat doc read-only until the host's seed
    /// flips the chat to chat2. The staleness checks compare this against
    /// the registry — inferring mode from `chat2_local_sub` misread
    /// edge-less chat2 handles (no subscription is ever installed offline)
    /// as stale s2 and retired them on every open, dropping the doc out
    /// from under live runs.
    room_gen: u32,
    /// A threshold checkpoint POST is in flight (review H1: the quiesce
    /// tick must not stack concurrent full-snapshot uploads).
    checkpointing: Arc<AtomicBool>,
    /// Set when a chat2 seed replaced this handle's lineage on disk: every
    /// further snapshot save from this handle is a stale FAT doc that would
    /// clobber the thin one — retired handles never persist again (unless no
    /// thin lineage exists on disk at all; `save_snapshot` double-checks, so
    /// a doc that was never seeded can't lose its only copy).
    retired: AtomicBool,
    /// chat2 relay client (docs/chat2-sync.md C3) — populated once the
    /// registry names roomGen 2 for this chat and the join resolves.
    chat2: Mutex<Option<zeron_sync::ChatClient>>,
    /// Local commits made before the relay connects (the dial can take up
    /// to a minute; offline, forever): buffered here by the subscription
    /// below and drained into the client on join (review B3 — a user
    /// message typed during the dial must not silently never sync).
    chat2_pending_local: Mutex<Vec<Vec<u8>>>,
    /// Local-update feed into the chat2 client (drop = unsubscribe).
    chat2_local_sub: Mutex<Option<loro::Subscription>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Joined transcript watch — re-sent on every doc change (WatchDocMessages).
    ///
    /// Attach-time refresh: the mirror is only maintained while watched, so a
    /// doc that changed unwatched materializes here, once, instead of on every
    /// commit it sat through in the background.
    pub fn watch_messages(&self) -> watch::Receiver<Arc<Vec<SessionMessageEntry>>> {
        self.touch();
        // Attach is a user signal: verify a quiet room is actually alive
        // (a doc-wedged DO keeps answering pings while delivering nothing,
        // and the background probe cadence can be hours out). Coalescing
        // no-op on a healthy or recently-active room.
        if let Some(chat2) = lock(&self.chat2).as_ref() {
            chat2.probe();
        }
        // Subscribe BEFORE the dirty check: a commit racing this attach then
        // sees a live receiver and publishes, instead of re-marking dirty
        // after our refresh and leaving the new watcher a cleared mirror.
        let rx = self.messages_tx.subscribe();
        if self.mirror_dirty.load(Ordering::Acquire) {
            self.publish_messages();
        }
        rx
    }

    fn touch(&self) {
        self.last_access.store(now_ms(), Ordering::Relaxed);
    }

    pub fn connected(&self) -> bool {
        lock(&self.chat2).is_some()
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (zeron folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        self.mirror_dirty.store(false, Ordering::Release);
        match self.doc.read_entries() {
            Ok(entries) => {
                let joined = join_continuation_entries(entries);
                // send_replace: update the watch even with no subscribers yet, so a
                // late subscriber's first borrow sees the current transcript.
                self.messages_tx.send_replace(Arc::new(joined));
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }

    /// Per-commit publish path: unwatched docs just mark the mirror dirty —
    /// rebuilding a full transcript nobody reads was a per-tick cost on every
    /// open doc (and kept a second transcript copy hot).
    fn publish_messages_if_watched(&self) {
        if self.messages_tx.receiver_count() == 0 {
            self.mirror_dirty.store(true, Ordering::Release);
            // Shrink the stale mirror: watch_messages rebuilds on attach.
            self.messages_tx.send_replace(Arc::default());
        } else {
            self.publish_messages();
        }
    }

    /// Rough resident cost for the LRU budget.
    fn resident_estimate(&self) -> usize {
        (self.snapshot_bytes.load(Ordering::Relaxed) * RESIDENT_BYTES_PER_SNAPSHOT_BYTE)
            .max(DOC_RESIDENT_FLOOR_BYTES)
    }
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: Mutex::new(None),
                workspace: OnceLock::new(),
                repos: OnceLock::new(),
                shutdown: CancellationToken::new(),
                tasks: TaskTracker::new(),
                handles: Mutex::new(HashMap::new()),
                seeding: Mutex::new(HashSet::new()),
                seed_waiting: Mutex::new(HashSet::new()),
                drain_waiting: Mutex::new(HashSet::new()),
                uploads: OnceLock::new(),
                connectivity: OnceLock::new(),
                connectivity_started: AtomicBool::new(false),
                transfers: watch::channel(Vec::new()).0,
                connectivity_grace: Mutex::new(DegradeGrace::default()),
                executing: Mutex::new(HashSet::new()),
                links: OnceLock::new(),
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            }),
        }
    }

    /// Every background task rides the tracker, raced against the shutdown
    /// token: the loops' own exits stay authoritative in normal operation;
    /// the token is the retirement override.
    fn spawn_worker(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let cancel = self.inner.shutdown.clone();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = fut => {}
            }
        });
    }

    /// `spawn_worker` for sites that pre-resolve a runtime handle (callers
    /// reachable from bare sync contexts, where `tasks.spawn` would panic).
    fn spawn_worker_on(
        &self,
        runtime: &tokio::runtime::Handle,
        fut: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let cancel = self.inner.shutdown.clone();
        self.inner.tasks.spawn_on(
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = fut => {}
                }
            },
            runtime,
        );
    }

    /// The sessions engine, once wired. `None` before assembly or after
    /// `shutdown_workers` — callers treat both as "executor unavailable".
    fn sessions(&self) -> Option<SessionsEngine> {
        lock(&self.inner.sessions).clone()
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        {
            // First set wins (the OnceLock contract this slot replaced).
            let mut slot = lock(&self.inner.sessions);
            if slot.is_none() {
                *slot = Some(sessions);
            }
        }
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
    }

    /// Retire this host's workers (runtime replacement, e.g. sign-out): cancel
    /// and await every spawned task, drop every open chat handle (ending the
    /// weak-keyed room/join loops and watcher streams), and sever the sessions
    /// back-edge so the replaced engine graph can actually drop. Idempotent.
    pub async fn shutdown_workers(&self) {
        self.inner.shutdown.cancel();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
        // Snapshot open docs BEFORE releasing their handles: the handles map
        // holds the only strong doc refs, and an unflushed doc dies with it.
        self.flush_all();
        // Take the map under the lock, drop the handles outside it.
        let handles = std::mem::take(&mut *lock(&self.inner.handles));
        drop(handles);
        lock(&self.inner.seeding).clear();
        lock(&self.inner.seed_waiting).clear();
        lock(&self.inner.sessions).take();
    }

    /// Test-only retirement sentinel: reports true once the doc-host graph
    /// has actually been freed.
    #[doc(hidden)]
    pub fn retirement_probe(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        let weak = Arc::downgrade(&self.inner);
        Box::new(move || weak.upgrade().is_none())
    }

    /// Wire the repos engine (engine assembly) — worktree materialization for
    /// Run commands carrying a [`zeron_proto::WorktreeSpec`].
    pub fn set_repos(&self, repos: crate::repos::Repos) {
        let _ = self.inner.repos.set(repos);
    }

    /// Wire the uploads store (engine assembly) — `pending://` ref resolution
    /// and the transfer-read jail.
    pub fn set_uploads(&self, uploads: crate::uploads::Uploads) {
        let _ = self.inner.uploads.set(uploads);
    }

    /// Wire the peer-link cache (engine assembly, edge runtimes only) — the
    /// transport for queued attachment transfers to a remote host.
    pub fn set_links(&self, links: Arc<zeron_rpc::LinkCache>) {
        let _ = self.inner.links.set(links);
    }

    /// Re-evaluate every open chat's command queue NOW. Called after an
    /// upload commit lands bytes on this device: a Run deferred on those
    /// bytes (`pending://` refs not yet on disk) becomes executable the
    /// moment its transfer completes — event-driven, not timer luck.
    pub fn kick_drains(&self) {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let chats = workspace.watch_chats();
        if self.inner.workspace.set(workspace).is_ok() {
            self.spawn_cutover_watcher(chats);
            self.spawn_migration_sweep();
        }
    }

    /// Host migration sweep: proactively seed this device's own s2 chats
    /// onto chat2, one per tick. The lazy open()-triggered seed could never
    /// fire in real usage — idle chats are never opened, a headless host's
    /// only opens are nudges (which arrive exactly when a run starts), and
    /// viewing a chat used to pin it. Every safety gate stays (s2 room
    /// joined, 2s frontier quiet, no live writer, no pending commands,
    /// frontier seal at the flip) — only the trigger changes: the host owns
    /// migrating its chats. Aborted seeds retry after a gap; a migrated
    /// fleet makes the tick a cheap no-op scan.
    fn spawn_migration_sweep(&self) {
        const TICK: std::time::Duration = std::time::Duration::from_secs(30);
        const RETRY_GAP_MS: i64 = 10 * 60 * 1000;
        let host = self.clone();
        self.spawn_worker(async move {
            let mut attempted: HashMap<String, i64> = HashMap::new();
            loop {
                tokio::time::sleep(TICK).await;
                let Some(edge) = host.inner.config.edge.clone() else {
                    return; // edge-less engine: nothing to migrate onto
                };
                let Some(ws) = host.workspace() else { continue };
                let chats: Vec<zeron_proto::Chat> = ws.watch_chats().borrow().clone();
                let device = host.inner.config.device_id.clone();
                let now = now_ms();
                let candidate = chats.into_iter().find(|c| {
                    c.device_id == device
                        && c.room_gen.unwrap_or(1) < 2
                        && !attempted
                            .get(&c.id)
                            .is_some_and(|t| now - *t <= RETRY_GAP_MS)
                });
                let Some(chat) = candidate else { continue };
                attempted.insert(chat.id.clone(), now);
                // A cached (warm) handle never re-enters open()'s build path,
                // so arm the quiet-waiter directly; a cold chat opens, which
                // arms it on the way up.
                let cached = lock(&host.inner.handles).get(&chat.id).cloned();
                match cached {
                    Some(handle) => {
                        if handle.room_gen < 2 && !handle.retired.load(Ordering::Relaxed) {
                            tracing::debug!(chat = %chat.id, "migration sweep: arming seed on warm s2 chat");
                            host.spawn_chat2_seed_when_quiet(edge, &chat.id, &handle);
                        }
                    }
                    None => {
                        tracing::debug!(chat = %chat.id, "migration sweep: opening cold s2 chat to seed");
                        if let Err(err) = host.open(&chat.id) {
                            tracing::warn!(chat = %chat.id, error = %err, "migration sweep open failed");
                        }
                    }
                }
            }
        });
    }

    /// Live cutover convergence: when a registry chat row flips to roomGen 2
    /// while this device holds an s2-mode handle, retire it NOW — a viewer
    /// watching the chat at flip time otherwise stays frozen on the dead s2
    /// room until they happen to reopen (the host writes only to chat2 from
    /// the flip on). Dropping the handle ends the watch streams cleanly; the
    /// UI's transcript/standing watches resubscribe and the fresh open takes
    /// the chat2 adopt path. A handle with a LIVE local writer (a running
    /// turn's doc ref) is left alone — the host never flips mid-run, and a
    /// racing writer must never lose its doc out from under it.
    fn spawn_cutover_watcher(&self, mut chats: watch::Receiver<Vec<zeron_proto::Chat>>) {
        let host = self.clone();
        self.spawn_worker(async move {
            loop {
                if chats.changed().await.is_err() {
                    return; // workspace host gone (shutdown)
                }
                let flipped: Vec<String> = chats
                    .borrow_and_update()
                    .iter()
                    .filter(|c| c.room_gen.unwrap_or(1) >= 2)
                    .map(|c| c.id.clone())
                    .collect();
                if flipped.is_empty() {
                    continue;
                }
                let mut dropped: Vec<String> = Vec::new();
                let mut stuck_live: Vec<(String, Arc<ChatDocHandle>)> = Vec::new();
                {
                    let mut handles = lock(&host.inner.handles);
                    for chat_id in flipped {
                        let Some(handle) = handles.get(&chat_id) else {
                            continue;
                        };
                        if handle.room_gen >= 2 {
                            continue; // already chat2-mode
                        }
                        let live_writer = Arc::strong_count(&handle.doc) > 1;
                        if live_writer {
                            // A run is writing into this s2 doc while the
                            // registry already says chat2 — the born-gen2
                            // race (this open beat its own CreateChat mint,
                            // 2026-08-11: transcript never reached any other
                            // device). No thin lineage exists on disk yet,
                            // so retiring here would suppress the doc's only
                            // persistence and abort the quiet-seed. Leave it
                            // live; seed once the run quiesces — the seed
                            // posts the chat2 checkpoint, persists the thin
                            // lineage, and retires the handle itself.
                            stuck_live.push((chat_id, handle.clone()));
                            continue;
                        }
                        handle.retired.store(true, Ordering::Relaxed);
                        handles.remove(&chat_id);
                        tracing::info!(chat = %chat_id,
                            "s2 handle dropped on chat2 cutover; watchers resubscribe onto the new room");
                        dropped.push(chat_id);
                    }
                }
                for (chat_id, handle) in stuck_live {
                    if host.is_host(&chat_id)
                        && let Some(edge) = host.inner.config.edge.clone()
                    {
                        host.spawn_chat2_seed_when_quiet(edge, &chat_id, &handle);
                    }
                }
                // Watchers resubscribe on their own — but a NUDGE-opened
                // handle has none, and its s2 room never carried the queued
                // command anyway (the sender pushed to chat2). On a born-
                // chat2 chat the nudge beats the registry row by design
                // (direct HTTP vs room sync), so the first open lands here
                // and dying silently strands the first message until the
                // next nudge — every new remote session's first send sat
                // ~30s+ until the user re-sent (user report). If we host
                // the chat, reopen NOW: the fresh open dials the chat2
                // room and the change-driven drain executes the command.
                for chat_id in dropped {
                    if !host.is_host(&chat_id) {
                        continue;
                    }
                    match host.open(&chat_id) {
                        Ok(_) => tracing::info!(chat = %chat_id,
                            "reopened as chat2 after cutover drop (host, pending work possible)"),
                        Err(err) => tracing::warn!(chat = %chat_id, error = %err,
                            "chat2 reopen after cutover drop failed"),
                    }
                }
            }
        });
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// Open (or return) the chat's doc handle: load the local snapshot (or init fresh),
    /// start the change-driven task, and join the edge room when configured.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        // The registry names the sync room generation (docs/chat2-sync.md
        // M2): absent row / absent field = legacy s2. Read it BEFORE the
        // cached-handle check — a cached s2-mode handle for a chat another
        // device has since cut over to chat2 would otherwise serve its frozen
        // fat lineage forever (the host writes only to the chat2 room now;
        // this device's s2 room has gone permanently silent).
        let chat_row = self
            .workspace()
            .and_then(|w| w.chat(chat_id).ok().flatten());
        // A row that EXISTS without `roomGen` is a pre-cutover legacy chat
        // (gen 1). A MISSING row is a chat being born right now: its
        // CreateChat mint (which stamps roomGen 2) is racing this open —
        // the composer attaches the transcript watch before its own mutate
        // lands, and a nudge beats registry sync by design. Defaulting the
        // absent row to 1 minted brand-new s2 rooms post-cutover: the host
        // ran the whole session against a room no other device reads (they
        // follow the row's gen 2 to an empty chat2 room), the run's live doc
        // ref blocked every heal, and the transcript never synced anywhere
        // (2026-08-11).
        let registry_gen = match chat_row.as_ref() {
            Some(row) => row.room_gen.unwrap_or(1),
            None => 2,
        };
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                let stale = registry_gen >= 2 && handle.room_gen < 2;
                if (stale || handle.retired.load(Ordering::Relaxed)) && !self.pinned(handle) {
                    // A seed flipped this chat under a cached fat handle
                    // (review B1): drop it so this open converges onto the
                    // thin lineage + chat2 room. Retire only at the drop:
                    // marking a PINNED stale handle retired while it kept
                    // serving suppressed the only persistence a stuck-live
                    // doc had and aborted its quiet-seed (born-gen2 race,
                    // 2026-08-11) — the cutover watcher and the seed itself
                    // own converging pinned handles.
                    handle.retired.store(true, Ordering::Relaxed);
                    handles.remove(chat_id);
                } else {
                    handle.touch();
                    return Ok(handle.clone());
                }
            }
        }
        // B2/M5 guard: the LOCAL epoch is the second cutover signal. A crash
        // between the thin save and the registry flip (or a not-yet-synced
        // registry) must NOT route an epoch-2 doc back onto s2 — the s2
        // room's fat doc would merge into the unrelated thin lineage and
        // duplicate every message. Local epoch >= 2 forces the chat2 branch
        // and best-effort completes the flip.
        let stored = self.inner.store.load_snapshot_with_cursor(chat_id)?;
        let stored_epoch = stored.as_ref().map(|(_, _, e)| *e).unwrap_or(0);
        let room_gen = if stored_epoch >= crate::chat2_host::CHAT2_DOC_EPOCH {
            if registry_gen < 2
                && let Some(ws) = self.workspace()
            {
                let _ = ws.set_chat_room_gen(chat_id, 2);
                tracing::info!(chat = %chat_id,
                    "completed interrupted chat2 flip (local epoch 2, registry said s2)");
            }
            2
        } else {
            registry_gen
        };
        let mut snapshot_len = 0usize;
        let mut chat2_cursor = 0u64;
        let mut requeue_commands: Vec<SessionCommandEntry> = Vec::new();
        let doc = if room_gen >= 2 {
            match stored {
                Some((bytes, cursor, epoch)) if epoch >= crate::chat2_host::CHAT2_DOC_EPOCH => {
                    snapshot_len = bytes.len();
                    chat2_cursor = cursor;
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                Some((bytes, _cursor, epoch)) if self.inner.config.edge.is_none() => {
                    // Offline/edge-less: adopting would blank a readable
                    // transcript with no way to catch up (review B4). Keep
                    // the old doc read-only-ish; the adopt runs on the next
                    // online open.
                    tracing::info!(chat = %chat_id, old_epoch = epoch,
                        "chat2 adopt deferred (no edge configured)");
                    snapshot_len = bytes.len();
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                Some((bytes, _cursor, epoch)) => {
                    // M3 discard-and-adopt: this device's doc predates the
                    // chat2 lineage. Keep the old snapshot under a suffixed
                    // id for rollback, carry over OUR OWN unresolved
                    // commands, and start fresh — the chat2 catch-up
                    // (checkpoint + rows) repopulates the transcript. This
                    // is the self-repair path: no user action, ever.
                    tracing::info!(chat = %chat_id, old_epoch = epoch,
                        "chat2 adopt: discarding pre-chat2 local doc (rollback copy kept)");
                    let rollback_id = format!("{chat_id}.pre-chat2");
                    // A re-adopt after a mid-catch-up crash reruns this path
                    // with a near-empty doc under `chat_id` — the FIRST
                    // rollback copy is the real transcript; never overwrite
                    // it (review B5).
                    if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None)) {
                        let _ = self.inner.store.save_snapshot(&rollback_id, &bytes);
                    }
                    if let Ok(raw) = {
                        let old = loro::LoroDoc::new();
                        old.import(&bytes).map(|_| old)
                    } {
                        let old_doc = SessionDoc::from_doc(raw);
                        if let Ok(commands) = old_doc.read_commands() {
                            requeue_commands = commands
                                .into_iter()
                                .filter(|c| {
                                    c.status == SessionCommandStatus::Pending
                                        && c.issued_by == self.inner.config.device_id
                                })
                                .collect();
                        }
                    }
                    SessionDoc::init(chat_id)?
                }
                None => {
                    // Born on chat2 (or a cold reader's first open): stamp
                    // the epoch-2 lineage NOW. Plain snapshot saves preserve
                    // an existing row's epoch but default a NEW row to 0 —
                    // without this stamp, the next open reads "pre-chat2
                    // doc" and the M3 adopt DISCARDS everything written
                    // since (caught by the restart_resume suite: first-turn
                    // transcripts vanished on reopen).
                    let doc = SessionDoc::init(chat_id)?;
                    if let Ok(snapshot) = doc.export_snapshot() {
                        let _ = self.inner.store.save_snapshot_with_cursor(
                            chat_id,
                            &snapshot,
                            0,
                            crate::chat2_host::CHAT2_DOC_EPOCH,
                        );
                    }
                    doc
                }
            }
        } else {
            match stored {
                Some((bytes, _, _)) => {
                    snapshot_len = bytes.len();
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                None => SessionDoc::init(chat_id)?,
            }
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens (command queueing,
        // drains, nudges) never watch the transcript, and the first
        // watch_messages attach materializes it on demand.
        let (messages_tx, _) = watch::channel(Arc::default());

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            mirror_dirty: AtomicBool::new(true),
            last_access: AtomicI64::new(now_ms()),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            room_gen,
            retired: AtomicBool::new(false),
            checkpointing: Arc::new(AtomicBool::new(false)),
            chat2: Mutex::new(None),
            chat2_pending_local: Mutex::new(Vec::new()),
            chat2_local_sub: Mutex::new(None),
            _sub: sub,
        });
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                return Ok(existing.clone()); // racing open — keep the first
            }
            handles.insert(chat_id.to_string(), handle.clone());
        }

        // Edge room join — offline-tolerant AND supervised. `ChatClient` only
        // self-reconnects AFTER a first successful join; a one-shot attempt
        // here (the pre-LRU design) left the doc silently local-only until
        // app restart whenever the dial hit a transient gap — a post-wake
        // network, `Auth::token()` momentarily `None` around a refresh, an
        // edge deploy. The LRU made that dice-roll constant (every reopen),
        // and a watched doc is pinned against eviction, so nothing ever
        // retried: the exact "transcript frozen until restart" report.
        // Retry on the workspace host's capped, jittered backoff; a system
        // wake redials immediately; eviction/purge ends the loop via `weak`.
        if let Some(edge) = &self.inner.config.edge {
            if room_gen >= 2 {
                // Subscription BEFORE the dial (review B3): every local
                // commit lands in the client when connected, else in the
                // pending buffer the join drains — nothing composed during
                // (or before) the dial is lost to the room.
                let weak_push = Arc::downgrade(&handle);
                let sub = doc
                    .doc()
                    .subscribe_local_update(Box::new(move |bytes: &Vec<u8>| {
                        if let Some(handle) = weak_push.upgrade() {
                            // The buffer push happens WHILE HOLDING the client
                            // lock (verify pass: releasing it between the None
                            // check and the push let the join's store+drain
                            // slip between, orphaning the update forever).
                            let client_guard = lock(&handle.chat2);
                            match &*client_guard {
                                Some(client) => client.enqueue_update(bytes.clone()),
                                None => lock(&handle.chat2_pending_local).push(bytes.clone()),
                            }
                        }
                        true
                    }));
                *lock(&handle.chat2_local_sub) = Some(sub);
                // Re-queue survives the adopt: our own pending commands
                // become fresh entries in the new lineage (the
                // processed_commands ledger still guards double execution).
                // Committed AFTER the local-update subscription above — a
                // commit before it never enters the pending buffer or the
                // client, so the requeued command would sit in the local doc
                // and never reach the room (the host would never see it).
                for command in &requeue_commands {
                    let _ = doc.queue_command(command);
                }
                // First contact with the room (cursor 0): everything
                // committed BEFORE the subscription above — SessionDoc::
                // init's container/meta ops, an adopt's fresh doc — is
                // invisible to the push path, yet every later commit
                // causally DEPENDS on it. Rows built on unpushed deps import
                // into peers' loro pending-buffers and never materialize:
                // born-chat2 cross-device runs sat invisible on every other
                // device (host never saw the command, viewers never saw the
                // transcript). Push the doc's full update log as the join's
                // first batch; once acked the cursor moves and this never
                // re-arms.
                if chat2_cursor == 0 {
                    match doc
                        .doc()
                        .export(loro::ExportMode::updates(&loro::VersionVector::default()))
                    {
                        Ok(bytes) if !bytes.is_empty() => {
                            lock(&handle.chat2_pending_local).push(bytes);
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(chat = %chat_id, error = %err,
                                "chat2 first-contact export failed; peers may stall on missing deps");
                        }
                    }
                }
                self.spawn_chat2_join(edge.clone(), &handle, chat2_cursor);
            } else {
                // Straggler gen-1 chat (the s2 client is gone — post-cutover,
                // no device reads or writes an s2 room). The local fat doc
                // serves reads as-is; if we host the chat, seed it onto chat2
                // in the background and the flip converges every device.
                let is_host = chat_row
                    .as_ref()
                    .is_some_and(|c| c.device_id == self.inner.config.device_id);
                if is_host && chat_row.is_some() {
                    // Quiescent-only (review B1): a seed under a live run or
                    // watched transcript would strand everything written
                    // after the rebuild instant in a retired fat lineage.
                    self.spawn_chat2_seed_when_quiet(edge.clone(), chat_id, &handle);
                }
            }
        }
        self.spawn_worker(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        self.evict_over_budget();
        Ok(handle)
    }

    /// chat2 relay join (docs/chat2-sync.md C3): deadline on every dial,
    /// capped jittered backoff, wake redial — and the client resolves only
    /// after full catch-up (checkpoint + rows), so "joined" here means
    /// "transcript converged".
    fn spawn_chat2_join(&self, edge: EdgeConfig, handle: &Arc<ChatDocHandle>, cursor: u64) {
        let chat = handle.chat_id.clone();
        let doc = handle.doc.clone();
        let store = self.inner.store.clone();
        let http = self.inner.http.clone();
        let device = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(handle);
        let host = self.clone();
        let mut token_changes = edge.token_changes();
        self.spawn_worker(async move {
            let sink = Arc::new(crate::chat2_host::EngineChatSink::new(&doc, store, chat.clone()));
            // The sink holds only a Weak doc ref (a strong one made every
            // chat2 handle read as perma-pinned — LRU eviction dead); this
            // task's own strong ref dies when the join resolves.
            drop(doc);
            let fetcher = Arc::new(crate::chat2_host::EdgeCheckpointFetcher::new(
                http,
                edge.clone(),
                chat.clone(),
            ));
            let url = edge.room_url(format!("/chat2/{chat}/ws"));
            let mut wake = zeron_sync::wake::subscribe();
            // Sibling-dial successes end a backoff wait immediately, exactly
            // like the joined clients' own reconnect loops (chat_client.rs).
            // Without this, a NEW chat whose first joins hit a network blip
            // waited out the full accumulated backoff (→30s) while every
            // established room redialed instantly on recovery — fresh sends
            // to new sessions stalled while other chats hummed (2026-08-19
            // user report, reproduced on two networks).
            let mut online = zeron_sync::wake::subscribe_online();
            let mut backoff = crate::workspace_host::JOIN_RETRY_BASE;
            loop {
                if weak.upgrade().is_none() {
                    return; // evicted or purged while dialing
                }
                // Dual transport: WS dial + a plain-HTTPS pull/push seam
                // (rows GET / POST on the same bearer auth as the checkpoint
                // fetch) — bootstraps in ~1 RTT and keeps syncing at backoff
                // cadence on networks that never pass the WS upgrade. With
                // the transport, connect resolves immediately (local-first).
                let transport = Arc::new(crate::chat2_host::EdgeChatTransport::new(
                    host.inner.http.clone(),
                    edge.clone(),
                    chat.clone(),
                    device.clone(),
                ));
                let dial = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    zeron_sync::ChatClient::connect_via_transport(
                        url.clone(),
                        sink.clone(),
                        fetcher.clone(),
                        &device,
                        cursor,
                        transport,
                    ),
                )
                .await;
                match dial {
                    Ok(Ok(client)) => {
                        if edge.bearer().await.is_none() {
                            return;
                        }
                        let Some(handle) = weak.upgrade() else {
                            return; // evicted mid-dial: drop leaves the room
                        };
                        let mut events = client.events();
                        let mut lifecycle_events = client.events();
                        {
                            // Store + drain under ONE client-lock critical
                            // section: the subscription pushes to the buffer
                            // while holding this same lock, so every commit
                            // is either drained here or enqueued directly
                            // after — never dropped between (verify pass).
                            let mut client_slot = lock(&handle.chat2);
                            let pending: Vec<Vec<u8>> =
                                std::mem::take(&mut *lock(&handle.chat2_pending_local));
                            for update in pending {
                                client.enqueue_update(update);
                            }
                            *client_slot = Some(client);
                        }
                        tracing::info!(chat = %chat, "chat2 room joined (converged)");
                        // Bootstrap heal: a room with NO checkpoint can't
                        // cover its rows' causal deps for cold readers — a
                        // pre-0.1.34 first contact whose init batch never
                        // went up (every reader parks every row on missing
                        // deps, transcript invisible forever), or a host
                        // whose WS pushes strand. The checkpoint is the
                        // universal patch: full doc over plain HTTP. Checked
                        // once, shortly after join (an idle chat never hits
                        // the quiesce tick, so the tick can't be the only
                        // trigger).
                        if host.is_host(&chat) {
                            let host = host.clone();
                            let weak = weak.clone();
                            host.clone().spawn_worker(async move {
                                // With the pull-first transport the client
                                // constructs before any state answer — wait
                                // until the server's view is KNOWN (bounded)
                                // or the all-zero placeholder stats would
                                // misread as "no checkpoint" and upload a
                                // spurious full-doc heal on a thin link.
                                for _ in 0..40u32 {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    let Some(handle) = weak.upgrade() else { return };
                                    let known = lock(&handle.chat2)
                                        .as_ref()
                                        .is_some_and(|c| c.stats().server_known);
                                    if known {
                                        break;
                                    }
                                }
                                let Some(handle) = weak.upgrade() else { return };
                                let no_checkpoint = lock(&handle.chat2)
                                    .as_ref()
                                    .is_some_and(|c| c.stats().server_known && c.stats().checkpoint_size == 0);
                                let has_content = handle
                                    .doc
                                    .read_entries()
                                    .map(|e| !e.is_empty())
                                    .unwrap_or(false);
                                if no_checkpoint && has_content {
                                    tracing::info!(chat = %handle.chat_id,
                                        "chat2 room has rows but no checkpoint; posting bootstrap checkpoint");
                                    host.spawn_chat2_checkpoint(&handle, "bootstrap");
                                }
                            });
                        }
                        // Host recovery duties (C3): a wiped room needs a
                        // seed checkpoint or fresh readers see only
                        // post-reset rows; rejected pushes reach peers only
                        // through a checkpoint. Watcher dies with the handle.
                        if host.is_host(&chat) {
                            let host = host.clone();
                            let weak = weak.clone();
                            let chat = chat.clone();
                            host.clone().spawn_worker(async move {
                                use zeron_sync::chat_client::ChatEvent;
                                loop {
                                    match events.recv().await {
                                        Ok(ChatEvent::ServerReset) => {
                                            let Some(handle) = weak.upgrade() else { return };
                                            tracing::warn!(chat = %chat, "chat2 room reset; posting seed checkpoint");
                                            host.spawn_chat2_checkpoint(&handle, "server-reset");
                                        }
                                        Ok(ChatEvent::PushRejected) => {
                                            let Some(handle) = weak.upgrade() else { return };
                                            tracing::warn!(chat = %chat, "chat2 push rejected; compensating via checkpoint");
                                            host.spawn_chat2_checkpoint(&handle, "push-rejected");
                                        }
                                        Ok(_) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                    }
                                }
                            });
                        }
                        drop(handle);
                        if token_changes.is_none() {
                            return;
                        }
                        loop {
                            tokio::select! {
                                event = lifecycle_events.recv() => match event {
                                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                },
                                _ = crate::workspace_host::token_changed(&mut token_changes) => {
                                    if edge.bearer().await.is_none() {
                                        if let Some(handle) = weak.upgrade() {
                                            lock(&handle.chat2).take();
                                            lock(&handle.chat2_local_sub).take();
                                        }
                                        tracing::info!(chat = %chat,
                                            "chat2 credentials removed; leaving room");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(chat = %chat, error = %err,
                            backoff_ms = backoff.as_millis() as u64,
                            "chat2 join failed; retrying");
                    }
                    Err(_) => {
                        tracing::warn!(chat = %chat,
                            backoff_ms = backoff.as_millis() as u64,
                            "chat2 join timed out; retrying");
                    }
                }
                // Drain stale online events first: only successes DURING this
                // wait count, or our own last dial would cut every wait to
                // zero (same discipline as chat_client's wait_backoff).
                while online.try_recv().is_ok() {}
                tokio::select! {
                    _ = tokio::time::sleep(backoff + crate::workspace_host::join_retry_jitter()) => {
                        backoff = (backoff * 2).min(crate::workspace_host::JOIN_RETRY_CAP);
                    }
                    _ = wake.recv() => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                    _ = online.recv() => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                    _ = crate::workspace_host::token_changed(&mut token_changes) => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                }
            }
        });
    }

    /// Host-side chat2 seed (docs/chat2-sync.md M1/M2): rebuild thin, POST
    /// the seed checkpoint, persist the thin lineage locally, THEN flip the
    /// registry — each step idempotent, a crash before the flip leaves the
    /// chat on s2 and the next open retries (M5).
    /// Defer a chat2 seed until the chat is verifiably quiet: the s2 room has
    /// joined (the rebuild must include every row the room holds — seeding
    /// from the pre-backfill local doc forked other devices' rows into the
    /// retired lineage) and the doc frontier has stopped moving (a
    /// nudge-burst in flight — an incoming queued command — must land and
    /// execute first). Holds only a weak handle: eviction ends the wait.
    fn spawn_chat2_seed_when_quiet(
        &self,
        edge: EdgeConfig,
        chat_id: &str,
        handle: &Arc<ChatDocHandle>,
    ) {
        const TICK: std::time::Duration = std::time::Duration::from_millis(500);
        const QUIET_TICKS: u32 = 4; // 2s of frontier silence
        if !lock(&self.inner.seed_waiting).insert(chat_id.to_string()) {
            return; // a quiet-waiter is already armed for this chat
        }
        let host = self.clone();
        let chat = chat_id.to_string();
        let weak = Arc::downgrade(handle);
        self.spawn_worker(async move {
            async {
                // The old join-wait gate (seed only after the s2 room had
                // backfilled) is gone with the s2 client: no device writes
                // an s2 room anymore, so the host's local doc IS the
                // authority for a straggler gen-1 chat. Only the quiet gate
                // remains (no seed under a moving frontier).
                let mut quiet = 0u32;
                let mut last_vv: Option<Vec<u8>> = None;
                loop {
                    tokio::time::sleep(TICK).await;
                    let Some(handle) = weak.upgrade() else { return };
                    if handle.retired.load(Ordering::Relaxed) {
                        return;
                    }
                    let vv = handle.doc.doc().oplog_vv().encode();
                    if last_vv.as_ref() == Some(&vv) {
                        quiet += 1;
                        if quiet >= QUIET_TICKS {
                            let doc = handle.doc.clone();
                            drop(handle);
                            host.spawn_chat2_seed(edge, &chat, doc);
                            return;
                        }
                    } else {
                        quiet = 0;
                        last_vv = Some(vv);
                    }
                }
            }
            .await;
            // Cleared on EVERY exit path so an aborted wait (evicted handle,
            // room never joined, seed handed off) can re-arm later.
            lock(&host.inner.seed_waiting).remove(&chat);
        });
    }

    fn spawn_chat2_seed(&self, edge: EdgeConfig, chat_id: &str, doc: Arc<SessionDoc>) {
        {
            let mut seeding = lock(&self.inner.seeding);
            if !seeding.insert(chat_id.to_string()) {
                return; // seed already in flight
            }
        }
        let host = self.clone();
        let chat = chat_id.to_string();
        self.spawn_worker(async move {
            let outcome = host.seed_chat2(&edge, &chat, doc).await;
            lock(&host.inner.seeding).remove(&chat);
            match outcome {
                Ok(()) => {
                    tracing::info!(chat = %chat, "chat2 seeded; registry flipped to roomGen 2");
                }
                Err(err) => {
                    tracing::warn!(chat = %chat, error = %err,
                        "chat2 seed failed; chat stays on s2 (retries next open)");
                }
            }
        });
    }

    async fn seed_chat2(
        &self,
        edge: &EdgeConfig,
        chat_id: &str,
        doc: Arc<SessionDoc>,
    ) -> Result<(), String> {
        use base64::Engine as _;
        let vv_at_rebuild = doc.doc().oplog_vv().encode();
        let rebuilt = zeron_doc::rebuild::rebuild_thin_doc(&doc).map_err(|e| e.to_string())?;
        // The seed's own doc ref must be gone before the pinned re-check
        // below: `pinned` reads `Arc::strong_count(&handle.doc) > 1`, and
        // holding this clone made that true unconditionally — every seed
        // aborted "became active mid-seed" and the cutover never flipped a
        // single chat (v0.1.32 DOA).
        drop(doc);
        if !rebuilt.sidecar.is_empty() {
            // Sidecar PARKED (docs/chat2-sync.md A2): full outputs are not
            // uploaded; they survive in the rollback snapshot + run journal.
            tracing::info!(chat = %chat_id, payloads = rebuilt.sidecar.len(),
                "chat2 seed: sidecar parked; outputs stay local");
        }
        let snapshot = rebuilt.doc.export_snapshot().map_err(|e| e.to_string())?;
        let frontier = rebuilt.doc.doc().oplog_vv().encode();
        let bearer = edge.bearer().await.ok_or("signed out")?;
        let url = format!(
            "{}/chat2/{}/checkpoint?seqCovered=0",
            edge.url.trim_end_matches('/'),
            chat_id
        );
        let res = self
            .inner
            .http
            .post(&url)
            .bearer_auth(&bearer)
            .header(
                "x-chat2-frontier",
                base64::engine::general_purpose::STANDARD.encode(&frontier),
            )
            .body(snapshot.clone())
            .send()
            .await
            .map_err(|e| format!("seed checkpoint POST: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("seed checkpoint HTTP {}", res.status()));
        }
        // PINNED RE-CHECK before anything irreversible (review B1): if a
        // run/watcher attached during the rebuild+POST, abort — everything
        // they write would fork away from the thin lineage. The orphan
        // checkpoint in the chat2 room is harmless (wholly replaced by the
        // next seed's seqCovered=0 POST); the chat stays on s2 and the next
        // quiet open retries.
        {
            let handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                if self.seed_blocked(handle) {
                    return Err("chat became active mid-seed; aborted before flip".into());
                }
                // Frontier seal (review B1's TOCTOU): ANY doc movement since
                // the rebuild — a synced remote row, a local write that has
                // already released its doc ref — means the thin lineage is
                // missing it. Borrowed read (no Arc clone: that would trip
                // the pinned check we just passed).
                if handle.doc.doc().oplog_vv().encode() != vv_at_rebuild {
                    return Err("doc advanced mid-seed; aborted before flip".into());
                }
            }
        }
        // Rollback copy of the fat lineage BEFORE the thin one replaces it —
        // never overwriting an existing copy (review B5).
        let rollback_id = format!("{chat_id}.pre-chat2");
        if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None))
            && let Ok(Some(old)) = self.inner.store.load_snapshot(chat_id)
        {
            let _ = self.inner.store.save_snapshot(&rollback_id, &old);
        }
        self.inner
            .store
            .save_snapshot_with_cursor(chat_id, &snapshot, 0, crate::chat2_host::CHAT2_DOC_EPOCH)
            .map_err(|e| e.to_string())?;
        // Registry flip LAST — the cutover signal every device dials by.
        let flipped = self
            .workspace()
            .ok_or("no workspace host")?
            .set_chat_room_gen(chat_id, 2)
            .map_err(|e| e.to_string())?;
        if !flipped {
            return Err("chat row vanished during seed".into());
        }
        // The live handle still holds the FAT doc on the s2 room. Retire it:
        // it must never persist again (it would clobber the thin lineage);
        // drop it entirely when unpinned so the next open converges onto
        // chat2. A pinned (watched/running) handle keeps working against s2
        // until it closes — the flip is registry-side, readers already moved.
        let dropped = {
            let mut handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                handle.retired.store(true, Ordering::Relaxed);
                // Drop unless a live WRITER holds the doc. Watchers do not
                // keep the fat handle alive: their streams end with it and
                // they resubscribe onto the chat2 adopt path (the same
                // contract the cutover watcher enforces for remote flips).
                if Arc::strong_count(&handle.doc) == 1 {
                    handles.remove(chat_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        tracing::info!(chat = %chat_id, handle_dropped = dropped, "chat2 seed complete");
        Ok(())
    }

    /// Boot-time transcript salvage (born-gen2 aftermath, 2026-08-11): a chat
    /// we host whose chat2 doc has NO message entries while its run journal
    /// has events lost its transcript to a stuck s2 handle (the retired flag
    /// suppressed every snapshot save; the post-restart reopen born a blank
    /// lineage). The full fat doc still exists in the legacy s2 room — the
    /// stuck engine pushed every op into it until it died — and sometimes in
    /// a `.pre-chat2` rollback on disk. Re-append its entries (thinned) into
    /// the LIVE chat2 lineage as ordinary incremental updates: no lineage
    /// replacement, no checkpoint surgery, every device converges through
    /// the normal room flow. Idempotent: a doc with any message entry is
    /// never touched, and the salvage only runs on the hosting device.
    pub fn spawn_transcript_salvage(&self, journals_dir: std::path::PathBuf) {
        let host = self.clone();
        self.spawn_worker(async move {
            // Let boot settle (registry load, room joins) before sweeping.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let Some(ws) = host.workspace() else { return };
            let chats: Vec<zeron_proto::Chat> = ws.watch_chats().borrow().clone();
            for chat in chats {
                if chat.device_id != host.inner.config.device_id {
                    continue; // only the host owns its chats' history
                }
                if chat.room_gen.unwrap_or(1) < 2 {
                    continue; // still s2-mode: transcript lives in its room
                }
                let journal = journals_dir.join(format!("{}.jsonl", chat.id));
                let journaled = std::fs::metadata(&journal)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
                if !journaled {
                    continue; // never ran here — an empty doc is just new
                }
                if let Err(err) = host.salvage_chat_transcript(&chat.id).await {
                    tracing::warn!(chat = %chat.id, error = %err, "transcript salvage failed");
                }
            }
        });
    }

    async fn salvage_chat_transcript(&self, chat_id: &str) -> Result<(), String> {
        let handle = self.open(chat_id).map_err(|e| e.to_string())?;
        if !handle
            .doc()
            .read_entries()
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Ok(()); // transcript present — nothing lost
        }
        // Fat source: the M3 adopt's rollback copy on disk. (The other
        // source — the legacy s2 room — went away with the s2 client; any
        // transcript that existed only there was salvaged by earlier
        // releases or is reachable in the room's storage server-side.)
        let rollback_id = format!("{chat_id}.pre-chat2");
        let fat_bytes = self.inner.store.load_snapshot(&rollback_id).ok().flatten();
        let Some(bytes) = fat_bytes else {
            return Ok(()); // no fat lineage anywhere — genuinely empty chat
        };
        let raw = loro::LoroDoc::new();
        raw.import(&bytes).map_err(|e| e.to_string())?;
        let fat = SessionDoc::from_doc(raw);
        // Thin before appending (docs/chat2-sync.md A2): full outputs are
        // parked, exactly like a seed — they survive in the rollback copy
        // saved below and the run journal.
        let rebuilt = zeron_doc::rebuild::rebuild_thin_doc(&fat).map_err(|e| e.to_string())?;
        let entries = rebuilt.doc.read_entries().map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok(());
        }
        if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None)) {
            let _ = self.inner.store.save_snapshot(&rollback_id, &bytes);
        }
        // Re-check emptiness at the last instant: a run that started during
        // the room fetch must not get history interleaved under it.
        if !handle
            .doc()
            .read_entries()
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err("doc gained entries mid-salvage; aborted".into());
        }
        for entry in &entries {
            handle
                .doc()
                .push_message(entry)
                .map_err(|e| e.to_string())?;
        }
        tracing::info!(chat = %chat_id, entries = entries.len(),
            "transcript salvaged into chat2 lineage");
        Ok(())
    }

    /// chat2 host duties on the doc-quiesce tick (docs/chat2-sync.md C3):
    /// - threshold checkpoint: when the room's row log passes 512KB or 200
    ///   rows, post a full checkpoint so cold readers load one compact blob
    ///   instead of replaying the log (the alert-shaped growth bound);
    /// - tail sidecar: publish the last-64 transcript JSON for thin/instant
    ///   readers (the iOS fallback path).
    async fn chat2_maintenance(&self, handle: &Arc<ChatDocHandle>) {
        if handle.retired.load(Ordering::Relaxed) {
            return;
        }
        let stats = match &*lock(&handle.chat2) {
            Some(client) => client.stats(),
            None => return,
        };
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let chat_id = handle.chat_id.clone();
        // Tail publish: cheap, every quiesce tick.
        if let Ok(tail) =
            zeron_doc::materialize_tail(&handle.doc, now_ms(), zeron_doc::TAIL_MESSAGE_COUNT)
            && let Ok(body) = serde_json::to_vec(&tail)
        {
            let http = self.inner.http.clone();
            let edge_tail = edge.clone();
            let chat = chat_id.clone();
            self.spawn_worker(async move {
                let Some(bearer) = edge_tail.bearer().await else {
                    return;
                };
                let url = format!(
                    "{}/chat2/{}/tail",
                    edge_tail.url.trim_end_matches('/'),
                    chat
                );
                let _ = http
                    .put(&url)
                    .bearer_auth(&bearer)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await;
            });
        }
        // Threshold checkpoint (rowBytes > 512KB || rows > 200), one in
        // flight at a time (review H1).
        if stats.row_bytes <= 512 * 1024 && stats.row_count <= 200 {
            return;
        }
        self.spawn_chat2_checkpoint(handle, "threshold");
    }

    /// POST a full checkpoint for a chat2 room (one in flight per handle).
    /// Callers: the quiesce-tick threshold above, and the client recovery
    /// events (`ServerReset` — a wiped room needs a seed checkpoint or every
    /// fresh reader sees only post-reset rows; `PushRejected` — the rejected
    /// ops reach peers only through a checkpoint).
    fn spawn_chat2_checkpoint(&self, handle: &Arc<ChatDocHandle>, reason: &'static str) {
        use base64::Engine as _;
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let stats = match &*lock(&handle.chat2) {
            Some(client) => client.stats(),
            None => return,
        };
        let chat_id = handle.chat_id.clone();
        if handle
            .checkpointing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let in_flight = handle.checkpointing.clone();
        let Ok(snapshot) = handle.doc.export_snapshot() else {
            in_flight.store(false, Ordering::Release);
            return;
        };
        let frontier = handle.doc.doc().oplog_vv().encode();
        let seq_covered = stats.cursor;
        let http = self.inner.http.clone();
        let weak_note = Arc::downgrade(handle);
        self.spawn_worker(async move {
            let Some(bearer) = edge.bearer().await else {
                in_flight.store(false, Ordering::Release);
                return;
            };
            let url = format!(
                "{}/chat2/{}/checkpoint?seqCovered={}",
                edge.url.trim_end_matches('/'),
                chat_id,
                seq_covered
            );
            let size = snapshot.len() as u64;
            match http
                .post(&url)
                .bearer_auth(&bearer)
                .header(
                    "x-chat2-frontier",
                    base64::engine::general_purpose::STANDARD.encode(&frontier),
                )
                .body(snapshot)
                .send()
                .await
            {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat_id, seq_covered, reason, "chat2 checkpoint posted");
                    if let Some(handle) = weak_note.upgrade()
                        && let Some(client) = &*lock(&handle.chat2)
                    {
                        client.note_checkpoint(seq_covered, size);
                    }
                }
                Ok(res) => {
                    tracing::warn!(chat = %chat_id, status = res.status().as_u16(),
                        "chat2 checkpoint rejected");
                }
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "chat2 checkpoint POST failed");
                }
            }
            in_flight.store(false, Ordering::Release);
        });
    }

    /// LRU eviction: while the warm set exceeds [`WARM_DOC_CAP`] or the
    /// resident estimate exceeds `DOC_LRU_BYTE_BUDGET`, close the
    /// least-recently-touched unpinned docs. Pinned (never evicted):
    /// - watched docs (`messages_tx` has receivers — a UI transcript);
    /// - docs with a live writer (`Arc<SessionDoc>` held outside the handle —
    ///   a run streaming into it);
    /// - host-side docs with pending commands (the executor owes them work).
    ///
    /// Eviction flushes a final snapshot, so reopen loses nothing; missed
    /// remote updates re-arrive through the room join's VV backfill.
    fn evict_over_budget(&self) {
        let mut by_age: Vec<(i64, String)> = {
            let handles = lock(&self.inner.handles);
            handles
                .values()
                .map(|h| (h.last_access.load(Ordering::Relaxed), h.chat_id.clone()))
                .collect()
        };
        by_age.sort_unstable();
        for (last_access, chat_id) in by_age {
            if now_ms() - last_access < EVICT_MIN_IDLE_MS {
                // Sorted oldest-first: everything after this is younger.
                return;
            }
            let (count, estimate) = {
                let handles = lock(&self.inner.handles);
                (
                    handles.len(),
                    handles
                        .values()
                        .map(|h| h.resident_estimate())
                        .sum::<usize>(),
                )
            };
            if count <= WARM_DOC_CAP && estimate <= zeron_doc::DOC_LRU_BYTE_BUDGET {
                return;
            }
            let evicted = {
                let mut handles = lock(&self.inner.handles);
                match handles.get(&chat_id) {
                    Some(handle) if !self.pinned(handle) => handles.remove(&chat_id),
                    _ => None,
                }
            };
            if let Some(handle) = evicted {
                // Final flush outside the map lock; ≤1s of changes could be
                // pending in the snapshot debounce.
                self.save_snapshot(&handle);
                tracing::debug!(chat = %handle.chat_id, "doc evicted (LRU)");
            }
        }
    }

    /// Seed-specific activity gate: a live WRITER (a run's doc ref) or
    /// pending host commands block a seed — watchers do NOT. Pure readers
    /// cannot lose writes, and the flip drops the handle so their streams
    /// end and resubscribe onto the chat2 adopt path. `pinned()` below keeps
    /// counting watchers for EVICTION, where a watched doc must stay
    /// resident. (Watcher-pinned seeds made "open a chat to look at it"
    /// self-defeating: the act of viewing blocked its own migration.)
    fn seed_blocked(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: never flip blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    fn pinned(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if handle.messages_tx.receiver_count() > 0 {
            return true;
        }
        // The handle itself holds one doc ref; more means a live writer.
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: keep the doc, never evict blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    /// Probe every open chat's room (window-focus liveness sweep). Each
    /// room ignores the hint unless it has been broadcast-quiet ≥30s.
    pub fn probe_open_chats(&self) {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            // chat2 rooms verify liveness on user signals — a
            // deaf-but-ponging DO otherwise freezes a watched transcript
            // for the whole background probe quiet window.
            if let Some(chat2) = lock(&handle.chat2).as_ref() {
                chat2.probe();
            }
        }
    }

    /// Window-focus fast path: one cheap HTTP probe of the edge decides
    /// whether to un-park every reconnect backoff NOW (success → online
    /// event → immediate redials with fresh backoff) or to leave them
    /// backing off (failure — a dial can't succeed either, so don't burn
    /// the attempt). Recovery rides the "user looked at the app" event
    /// instead of timer luck.
    pub fn probe_edge_reachability(&self) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!("{}/health", edge.url.trim_end_matches('/'));
        let http = self.inner.http.clone();
        self.spawn_worker_on(&runtime, async move {
            let res = http
                .get(&url)
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await;
            if let Ok(res) = res
                && res.status().is_success()
            {
                zeron_sync::wake::notify_online();
            }
        });
    }

    /// The in-flight queued-attachment transfer set: current entries first,
    /// then a fresh snapshot per landed chunk (see `push_attachments`).
    pub fn watch_transfers(&self) -> watch::Receiver<Vec<zeron_proto::TransferProgress>> {
        self.inner.transfers.subscribe()
    }

    /// Publish one transfer's progress (upserted by uploadId).
    fn transfer_progress_set(&self, upload_id: &str, file_name: &str, done: u64, total: u64) {
        self.inner.transfers.send_modify(|list| {
            match list.iter_mut().find(|t| t.upload_id == upload_id) {
                Some(t) => {
                    t.done = done;
                    t.total = total;
                }
                None => list.push(zeron_proto::TransferProgress {
                    upload_id: upload_id.to_string(),
                    file_name: file_name.to_string(),
                    done,
                    total,
                }),
            }
        });
    }

    /// Retire a transfer's progress entry (commit landed, or the attempt
    /// failed and the retry will re-publish).
    fn transfer_progress_clear(&self, upload_id: &str) {
        self.inner.transfers.send_modify(|list| {
            list.retain(|t| t.upload_id != upload_id);
        });
    }

    /// The connectivity stream: current posture first, then every change.
    /// Lazily starts a monitor — a 1s recompute over in-memory stats
    /// (atomics + small locks), published only when the value changes. The
    /// retry countdown renders client-side from `retry_at_ms`, so quiet
    /// periods emit nothing at all.
    pub fn watch_connectivity(&self) -> watch::Receiver<zeron_proto::Connectivity> {
        let tx = self
            .inner
            .connectivity
            .get_or_init(|| watch::channel(self.compute_connectivity()).0);
        let rx = tx.subscribe();
        if !self.inner.connectivity_started.swap(true, Ordering::SeqCst)
            && tokio::runtime::Handle::try_current().is_ok()
        {
            let host = self.clone();
            self.spawn_worker(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let next = host.compute_connectivity();
                    if let Some(tx) = host.inner.connectivity.get() {
                        tx.send_if_modified(|cur| {
                            if *cur == next {
                                false
                            } else {
                                *cur = next;
                                true
                            }
                        });
                    }
                }
            });
        }
        rx
    }

    /// One snapshot of the edge posture: OS path status beats registry-room
    /// state beats per-chat rooms. `Disabled` (the default) = local profile.
    ///
    /// Degradation is HYSTERETIC (v0.2.12 feedback): a room mid-join, an
    /// idle link waking for a send, or a navigation-triggered dial all read
    /// "disconnected" for a few hundred ms on a healthy network — surfacing
    /// those flashed amber warnings and "Queued" badges at every chat
    /// switch. Raw degradation must persist [`DEGRADE_GRACE`] before it is
    /// reported; recovery reports instantly.
    fn compute_connectivity(&self) -> zeron_proto::Connectivity {
        use zeron_proto::{ChatConnectivity, Connectivity, ConnectivityState};
        let workspace = self.workspace();
        let edge_expected = self.inner.config.edge.is_some()
            || workspace.as_ref().is_some_and(|w| w.edge_expected());
        if !edge_expected {
            return Connectivity::default();
        }
        let now = std::time::Instant::now();
        let mut grace = lock(&self.inner.connectivity_grace);
        let statuses = self.sync_statuses();
        grace.retain_chats(|id| statuses.iter().any(|(chat_id, _)| chat_id == id));
        let chats = statuses
            .into_iter()
            .map(|(chat_id, stats)| {
                let stats = stats.unwrap_or_default();
                let connected = !grace.degraded(GraceKey::Chat(&chat_id), !stats.connected, now);
                ChatConnectivity {
                    chat_id,
                    connected,
                    pending_pushes: stats.pending_pushes,
                }
            })
            .collect();
        let reconnect = workspace.as_ref().and_then(|w| w.reconnect_state());
        let registry_connected = workspace
            .as_ref()
            .and_then(|w| w.sync_status())
            .is_some_and(|s| s.connected);
        let path_offline =
            grace.degraded(GraceKey::OsPath, zeron_sync::wake::path_is_offline(), now);
        let registry_down = grace.degraded(GraceKey::Registry, !registry_connected, now);
        let (state, retry_at_ms, last_failure) = if path_offline {
            (
                ConnectivityState::Offline,
                0,
                reconnect.and_then(|r| r.last_failure),
            )
        } else if !registry_down {
            (ConnectivityState::Connected, 0, None)
        } else {
            let reconnect = reconnect.unwrap_or_default();
            (
                ConnectivityState::Reconnecting,
                reconnect.retry_at_ms,
                reconnect.last_failure,
            )
        };
        Connectivity {
            state,
            retry_at_ms,
            last_failure,
            chats,
        }
    }

    /// Per-open-chat room introspection for SyncStatus / `zeron sync`.
    /// `None` room = still dialing (join retry loop) or edge-less.
    pub fn sync_statuses(&self) -> Vec<(String, Option<zeron_sync::ChatStatsSnapshot>)> {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        let mut rows: Vec<(String, Option<zeron_sync::ChatStatsSnapshot>)> = handles
            .iter()
            .map(|h| {
                (
                    h.chat_id.clone(),
                    lock(&h.chat2).as_ref().map(|client| client.stats()),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// Drop a chat's doc unconditionally and delete its local snapshot — the
    /// chat is gone (DeleteChat / DeleteSpace cascade). Watchers see the
    /// stream end; a racing writer keeps its orphaned doc until the run ends.
    pub fn purge_chat(&self, chat_id: &str) {
        let removed = lock(&self.inner.handles).remove(chat_id);
        drop(removed);
        if let Err(err) = self.inner.store.delete_snapshot(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
        }
    }

    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        self.queue_command_with_transfers(chat_id, payload, Vec::new())
    }

    /// [`Self::queue_command`] plus queued-attachment transfers: the command's
    /// `pending://` refs name bytes already committed to THIS device's uploads
    /// dir; when another device hosts the chat, a background task pushes them
    /// over the peer link (retry until they land) while the command is
    /// already durably queued. The send is a local write — attachment bytes
    /// chase it, never gate it (2026-08-19 incident).
    pub fn queue_command_with_transfers(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
        transfers: Vec<crate::uploads::AttachmentTransfer>,
    ) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        let id = new_id();
        let now = now_ms();
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        let is_message = matches!(
            payload,
            SessionCommandPayload::Run { .. } | SessionCommandPayload::Steer { .. }
        );
        let entry = SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        handle.doc.queue_command(&entry)?;
        // Sending a message revives an archived chat: the user is acting in it
        // again, so the LWW row flips back to active on every device. Best-
        // effort — the command itself is durable regardless.
        if is_message {
            if let Some(workspace) = self.workspace() {
                match workspace.chat(chat_id) {
                    Ok(Some(chat)) if chat.archived => {
                        if let Err(err) = workspace.set_chat_archived(chat_id, false) {
                            tracing::warn!(chat = %chat_id, error = %err, "unarchive on send failed");
                        }
                    }
                    _ => {}
                }
            }
        }
        // §7 durable delivery: when another device hosts this chat, nudge its device
        // room so a cold host opens the doc and drains the queue. Fire-and-forget —
        // the command is durable in the doc either way (a host that opens the chat
        // for any other reason still executes it).
        self.nudge_remote_host(chat_id);
        self.spawn_command_delivery(chat_id, entry, transfers);
        Ok(id)
    }

    /// POST `{edge}/device/{host}/nudge {chatId}` when the chat's workspace row names
    /// another device as host. Best-effort: offline/edge-less engines skip silently.
    fn nudge_remote_host(&self, chat_id: &str) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Some(workspace) = self.workspace() else {
            return;
        };
        let host_device = match workspace.chat(chat_id) {
            Ok(Some(chat)) => chat.device_id,
            // Unclaimed chat: whoever drains first claims it — nobody to nudge.
            _ => return,
        };
        if host_device == self.inner.config.device_id {
            return;
        }
        // Only meaningful inside a runtime (RPC handlers, executors); bare sync
        // callers (unit tests) skip rather than panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!(
            "{}/device/{}/nudge",
            edge.url.trim_end_matches('/'),
            host_device
        );
        let chat = chat_id.to_string();
        self.spawn_worker_on(&runtime, async move {
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::warn!(chat = %chat, "nudge skipped: signed out");
                return;
            };
            let send = reqwest::Client::new()
                .post(&url)
                .bearer_auth(&bearer)
                .json(&serde_json::json!({ "chatId": chat }))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match send {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat, device = %host_device, "host nudged");
                }
                Ok(res) => tracing::warn!(chat = %chat, device = %host_device,
                    status = res.status().as_u16(), "nudge rejected"),
                Err(err) => {
                    tracing::warn!(chat = %chat, error = %err, "nudge failed (best-effort)")
                }
            }
        });
    }

    /// The chat's host device when it is NOT this engine (mirrors
    /// `nudge_remote_host`'s ownership read).
    fn remote_host_for(&self, chat_id: &str) -> Option<String> {
        let workspace = self.workspace()?;
        let host_device = match workspace.chat(chat_id) {
            Ok(Some(chat)) => chat.device_id,
            _ => return None,
        };
        (host_device != self.inner.config.device_id).then_some(host_device)
    }

    /// Durable-delivery escort for one queued command aimed at a REMOTE host:
    ///
    /// 1. push any queued attachment bytes over the peer link (retry until
    ///    they land — the relayed command must never outrun its bytes);
    /// 2. give the normal path (chat2 rows → edge → host's room) a short
    ///    grace to ack;
    /// 3. rows still not at the edge but the peer link alive → relay-forward
    ///    the entry itself ([`zeron_rpc::methods::RELAY_COMMAND`]). The
    ///    host's processed ledger claims the client-minted id, so the doc
    ///    row arriving later dedupes to a no-op — exactly-once by
    ///    construction (the 2026-08-18 03:45 incident shape: nudges flowed,
    ///    rows didn't; there was no second road for the command).
    ///
    /// Stops the moment any path lands. No-op for locally-hosted chats.
    fn spawn_command_delivery(
        &self,
        chat_id: &str,
        entry: SessionCommandEntry,
        transfers: Vec<crate::uploads::AttachmentTransfer>,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return; // bare sync callers (unit tests) skip rather than panic
        };
        let host = self.clone();
        let chat = chat_id.to_string();
        self.spawn_worker_on(&runtime, async move {
            let Some(target) = host.remote_host_for(&chat) else {
                return; // local host (or no row yet claimed remotely)
            };
            if !transfers.is_empty() && !host.deliver_attachments(&chat, &transfers).await {
                return; // gave up; the drain's wait cap surfaces the failure
            }
            let mut wake = zeron_sync::wake::subscribe();
            let mut online = zeron_sync::wake::subscribe_online();
            let give_up = tokio::time::Instant::now() + RELAY_GIVE_UP;
            let grace_end = tokio::time::Instant::now() + ROWS_GRACE;
            while tokio::time::Instant::now() < grace_end {
                if host.rows_flushed(&chat) {
                    return; // rows on the edge — the normal path has it
                }
                tokio::time::sleep(ROWS_POLL).await;
            }
            let mut backoff = RELAY_BACKOFF_BASE;
            loop {
                if host.rows_flushed(&chat) {
                    return; // the normal path won while we were retrying
                }
                match host.relay_command(&target, &chat, &entry).await {
                    Ok(outcome) => {
                        tracing::info!(chat = %chat, device = %target, command = %entry.id,
                            outcome, "command delivered via peer relay");
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat, device = %target, error = %err,
                            backoff_ms = backoff.as_millis() as u64,
                            "peer-relay delivery retrying");
                    }
                }
                if tokio::time::Instant::now() >= give_up {
                    tracing::warn!(chat = %chat, command = %entry.id,
                        "peer-relay delivery gave up; command remains queued in the doc");
                    return;
                }
                while wake.try_recv().is_ok() {}
                while online.try_recv().is_ok() {}
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wake.recv() => {}
                    _ = online.recv() => {}
                }
                backoff = (backoff * 2).min(RELAY_BACKOFF_CAP);
            }
        });
    }

    /// Step 1 of the escort: push staged bytes until they land (event-driven
    /// backoff), `true` on success. Re-resolves the host each attempt (a
    /// claim can move the chat).
    async fn deliver_attachments(
        &self,
        chat: &str,
        transfers: &[crate::uploads::AttachmentTransfer],
    ) -> bool {
        let mut wake = zeron_sync::wake::subscribe();
        let mut online = zeron_sync::wake::subscribe_online();
        let mut backoff = TRANSFER_BACKOFF_BASE;
        let deadline = tokio::time::Instant::now() + ATTACHMENT_WAIT_MAX;
        loop {
            let Some(target) = self.remote_host_for(chat) else {
                return true; // became locally hosted: bytes already here
            };
            match self.push_attachments(&target, transfers).await {
                Ok(()) => {
                    tracing::info!(chat = %chat, device = %target,
                        count = transfers.len(), "queued attachments delivered");
                    // The bytes beat the drain's next look — kick it via the
                    // durable nudge (the host's UploadCommit already kicked
                    // its local drains too).
                    self.nudge_remote_host(chat);
                    return true;
                }
                Err(TransferError::Permanent(err)) => {
                    tracing::warn!(chat = %chat, device = %target, error = %err,
                        "queued attachment transfer failed permanently");
                    return false;
                }
                Err(TransferError::Transient(err)) => {
                    tracing::warn!(chat = %chat, device = %target, error = %err,
                        backoff_ms = backoff.as_millis() as u64,
                        "queued attachment transfer retrying");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(chat = %chat, "queued attachment transfer gave up (wait cap)");
                return false;
            }
            while wake.try_recv().is_ok() {}
            while online.try_recv().is_ok() {}
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = wake.recv() => {}
                _ = online.recv() => {}
            }
            backoff = (backoff * 2).min(TRANSFER_BACKOFF_CAP);
        }
    }

    /// Every local chat2 batch acked while connected — our rows are ON the
    /// edge, and the host's own room connection will deliver them. (A room
    /// that hasn't joined keeps pre-join updates in a local buffer, so
    /// `connected` is load-bearing here, not just the empty queue.)
    fn rows_flushed(&self, chat_id: &str) -> bool {
        let handle = lock(&self.inner.handles).get(chat_id).cloned();
        handle
            .and_then(|h| lock(&h.chat2).as_ref().map(|c| c.stats()))
            .is_some_and(|s| s.connected && s.pending_pushes == 0)
    }

    /// One relay attempt: version-gate the host, then forward the entry over
    /// the peer link. Timeouts mark the link suspect (drop + redial next
    /// attempt); a host-side refusal is permanent for THIS attempt but the
    /// escort keeps retrying until the give-up cap (the refusal may be
    /// "attachments not landed yet").
    async fn relay_command(
        &self,
        target: &str,
        chat_id: &str,
        entry: &SessionCommandEntry,
    ) -> Result<&'static str, String> {
        let supported = self
            .workspace()
            .and_then(|ws| ws.read_devices().ok())
            .into_iter()
            .flatten()
            .find(|d| d.id == target)
            .and_then(|d| d.version.as_deref().and_then(zeron_proto::version_triple))
            .is_some_and(|v| v >= RELAY_MIN_VERSION);
        if !supported {
            return Err("host does not support relay delivery (version gate)".into());
        }
        let links = self
            .inner
            .links
            .get()
            .ok_or_else(|| "peer links not wired".to_string())?;
        let client = links
            .client(target)
            .await
            .map_err(|e| format!("peer link: {e}"))?;
        let params = serde_json::json!({ "chatId": chat_id, "entry": entry });
        let call = client.call(zeron_rpc::methods::RELAY_COMMAND, params);
        match tokio::time::timeout(RELAY_CALL_TIMEOUT, call).await {
            Err(_) => {
                links.invalidate(target);
                Err("relay call timed out; peer link suspect".into())
            }
            Ok(Err(zeron_rpc::RpcError::Failed(err))) => Err(format!("host refused: {err}")),
            Ok(Err(err)) => {
                links.invalidate(target);
                Err(format!("relay call failed: {err}"))
            }
            Ok(Ok(reply)) => Ok(match reply.get("outcome").and_then(|v| v.as_str()) {
                Some("executed") => "executed",
                Some("duplicate") => "duplicate",
                Some("expired") => "expired",
                Some("superseded") => "superseded",
                _ => "accepted",
            }),
        }
    }

    /// User-driven retry (the failed-send affordance): re-kick every
    /// delivery road for a chat whose queued sends haven't been adopted —
    /// fresh chat2 socket (a zombie room is the usual suspect), host nudge,
    /// a local drain pass, and a fresh delivery escort per still-pending
    /// command with its attachment transfers re-derived from the entries'
    /// `pending://` refs (idempotent: re-pushing landed bytes re-commits the
    /// same file; the processed ledger keeps execution exactly-once).
    pub fn retry_delivery(&self, chat_id: &str) -> Result<(), EngineError> {
        let handle = self.open(chat_id)?;
        if let Some(chat2) = lock(&handle.chat2).as_ref() {
            chat2.redial();
        }
        self.nudge_remote_host(chat_id);
        let commands = handle.doc.read_commands()?;
        let pending: Vec<SessionCommandEntry> = commands
            .iter()
            .filter(|c| {
                c.status == SessionCommandStatus::Pending
                    && !self.inner.store.is_processed(&c.id).unwrap_or(false)
            })
            .cloned()
            .collect();
        for entry in pending {
            let transfers = command_transfers(&entry);
            self.spawn_command_delivery(chat_id, entry, transfers);
        }
        // Dead attempts: a Run/Steer whose user message never landed and
        // whose command can never execute again — Rejected (execute failed,
        // or the dead-command sweep terminalized it), or consumed by the
        // ledger with no outcome and not currently executing (crash between
        // mark and resolve). Exactly-once is per command ID, so a
        // user-driven retry mints a FRESH attempt: new id, same payload and
        // message id (the executor's user-entry pre-write dedupes by message
        // id). One re-issue per message — the LATEST attempt speaks for it.
        let messages = handle.doc.read_entries().unwrap_or_default();
        let message_landed = |mid: &str| messages.iter().any(|m| m.id == mid);
        let mut latest_dead: HashMap<String, &SessionCommandEntry> = HashMap::new();
        for c in &commands {
            let Some(mid) = (match &c.payload {
                SessionCommandPayload::Run { message_id, .. } => Some(message_id.as_str()),
                SessionCommandPayload::Steer { message_id, .. } => message_id.as_deref(),
                _ => None,
            }) else {
                continue;
            };
            if message_landed(mid) {
                continue;
            }
            let dead = match c.status {
                // Rejected: execute failed or the sweep terminalized a crash
                // window. Expired: the entry outlived its TTL undelivered —
                // an explicit user retry is exactly the consent to re-send.
                SessionCommandStatus::Rejected | SessionCommandStatus::Expired => true,
                SessionCommandStatus::Pending => {
                    self.inner.store.is_processed(&c.id).unwrap_or(false)
                        && !lock(&self.inner.executing).contains(&c.id)
                }
                _ => false,
            };
            if !dead {
                continue;
            }
            // A LIVE pending attempt for the same message (queued or being
            // escorted above) makes a re-issue a duplicate — skip.
            let live_attempt = commands.iter().any(|o| {
                o.id != c.id
                    && o.status == SessionCommandStatus::Pending
                    && !self.inner.store.is_processed(&o.id).unwrap_or(false)
                    && match (&o.payload, &c.payload) {
                        (
                            SessionCommandPayload::Run { message_id: a, .. },
                            SessionCommandPayload::Run { message_id: b, .. },
                        ) => a == b,
                        (
                            SessionCommandPayload::Steer { message_id: a, .. },
                            SessionCommandPayload::Steer { message_id: b, .. },
                        ) => a == b,
                        _ => false,
                    }
            });
            if live_attempt {
                continue;
            }
            match latest_dead.entry(mid.to_string()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    if c.issued_at > slot.get().issued_at {
                        slot.insert(c);
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(c);
                }
            }
        }
        for old in latest_dead.values() {
            if old.status == SessionCommandStatus::Pending {
                // Terminalize the consumed-but-dead original so the doc tells
                // the truth and the next retry pass doesn't see it again.
                self.resolve_command(
                    &handle,
                    &old.id,
                    SessionCommandStatus::Rejected,
                    Some("interrupted before completion — superseded by retry"),
                );
            }
            let now = now_ms();
            let reissue = SessionCommandEntry {
                id: new_id(),
                payload: old.payload.clone(),
                issued_by: self.inner.config.device_id.clone(),
                issued_at: now,
                based_on: messages.last().map(|m| CommandBasedOn {
                    turn_id: Some(m.id.clone()),
                    frontier: None,
                }),
                expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
                status: SessionCommandStatus::Pending,
                resolution: None,
            };
            tracing::info!(chat = %chat_id, old = %old.id, new = %reissue.id,
                "retry re-issues a dead send attempt");
            handle.doc.queue_command(&reissue)?;
            let transfers = command_transfers(&reissue);
            self.spawn_command_delivery(chat_id, reissue, transfers);
        }
        // Locally-hosted (or already-synced) commands: a drain pass is the
        // whole retry.
        if tokio::runtime::Handle::try_current().is_ok() {
            let host = self.clone();
            let handle = handle.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
        Ok(())
    }

    /// Host side of [`zeron_rpc::methods::RELAY_COMMAND`]: evaluate the
    /// forwarded entry against OUR doc (dedupe/TTL/supersede rules apply
    /// unchanged), claim its client-minted id in the processed ledger, then
    /// execute. The claim is what makes the doc row arriving later — over
    /// chat2 sync — a no-op in the drain: exactly-once across both roads.
    pub async fn ingest_relayed_command(
        &self,
        chat_id: &str,
        entry: SessionCommandEntry,
    ) -> Result<&'static str, EngineError> {
        let handle = self.open(chat_id)?;
        // The sender sequences attachment transfers BEFORE the relay; refuse
        // (retryably) rather than run without the images.
        if !self.missing_attachments(&entry).is_empty() {
            return Err(EngineError::Other("attachments not landed yet".into()));
        }
        let sessions = self
            .sessions()
            .ok_or_else(|| EngineError::Other("executor unavailable".into()))?;
        let commands = handle.doc.read_commands()?;
        let messages = handle.doc.read_entries().unwrap_or_default();
        let current_turn_id = messages.last().map(|m| m.id.clone());
        let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
        let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
        let disposition = evaluate_command(
            &entry,
            &EvaluationContext {
                is_processed: &is_processed,
                now_ms: now_ms(),
                entries: &commands,
                current_turn_id: current_turn_id.as_deref(),
                turn_is_past: &turn_is_past,
            },
        );
        if matches!(disposition, CommandDisposition::Skip) {
            return Ok("duplicate");
        }
        // In-flight claim first (the drain's dead-command sweep must see this
        // id as alive, not crashed, while the execute below runs).
        if !lock(&self.inner.executing).insert(entry.id.clone()) {
            return Ok("duplicate");
        }
        // Claim BEFORE executing (the drain's own mark-before-execute rule).
        let marked = self.inner.store.mark_processed(&entry.id);
        let result = match marked {
            Err(err) => Err(err.into()),
            Ok(false) => Ok("duplicate"),
            Ok(true) => match disposition {
                CommandDisposition::Skip => Ok("duplicate"),
                CommandDisposition::Expired => Ok("expired"),
                CommandDisposition::Superseded => Ok("superseded"),
                CommandDisposition::Execute => match self.execute(&sessions, &handle, &entry).await
                {
                    Ok(_) => Ok("executed"),
                    Err(err) => Err(err),
                },
            },
        };
        lock(&self.inner.executing).remove(&entry.id);
        result
    }

    /// One transfer attempt: chunked `UploadChunk` + `UploadCommit` straight
    /// over the peer link (same wire the UI's legacy path used, so old and
    /// new engines interoperate). Timeouts mark the link suspect —
    /// `invalidate` drops the cached socket so the retry dials fresh instead
    /// of feeding a zombie pipe forever (2026-08-19 incident).
    async fn push_attachments(
        &self,
        target: &str,
        transfers: &[crate::uploads::AttachmentTransfer],
    ) -> Result<(), TransferError> {
        use TransferError::{Permanent, Transient};
        let Some(links) = self.inner.links.get() else {
            return Err(Permanent("peer links not wired".into()));
        };
        let Some(uploads) = self.inner.uploads.get() else {
            return Err(Permanent("uploads not wired".into()));
        };
        let client = links
            .client(target)
            .await
            .map_err(|e| Transient(format!("peer link: {e}")))?;
        for transfer in transfers {
            // Bytes come from the uploads jail only — a transfer names an
            // upload identity, never an arbitrary path.
            let source = uploads.pending_target(&transfer.upload_id, &transfer.file_name);
            let bytes = tokio::fs::read(&source)
                .await
                .map_err(|e| Permanent(format!("staged attachment missing: {e}")))?;
            // Progress entry for the sender's thumbnail ring, updated per
            // landed chunk. The guard retires it on EVERY exit — commit,
            // timeout, refusal — so a dead attempt falls back to the
            // indeterminate spinner and the retry re-publishes from 0.
            let total = bytes.len() as u64;
            self.transfer_progress_set(&transfer.upload_id, &transfer.file_name, 0, total);
            let _progress = TransferProgressGuard {
                host: self,
                upload_id: &transfer.upload_id,
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let mut start = 0usize;
            let mut seq = 0u64;
            loop {
                let end = (start + TRANSFER_CHUNK_B64).min(b64.len());
                let params = serde_json::json!({
                    "uploadId": transfer.upload_id, "seq": seq, "data": &b64[start..end],
                });
                let call = client.call(zeron_rpc::methods::UPLOAD_CHUNK, params);
                match tokio::time::timeout(TRANSFER_CHUNK_TIMEOUT, call).await {
                    Err(_) => {
                        links.invalidate(target);
                        return Err(Transient("chunk push timed out; peer link suspect".into()));
                    }
                    Ok(Err(zeron_rpc::RpcError::Failed(err))) => {
                        return Err(Permanent(format!("host refused chunk: {err}")));
                    }
                    Ok(Err(err)) => {
                        links.invalidate(target);
                        return Err(Transient(format!("chunk push failed: {err}")));
                    }
                    Ok(Ok(_)) => {}
                }
                start = end;
                seq += 1;
                // b64 → raw: 4 chars carry 3 bytes; min-clamp absorbs the
                // final chunk's padding overshoot.
                let done = ((start as u64) * 3 / 4).min(total);
                self.transfer_progress_set(&transfer.upload_id, &transfer.file_name, done, total);
                if start >= b64.len() {
                    break;
                }
            }
            let params = serde_json::json!({
                "uploadId": transfer.upload_id, "fileName": transfer.file_name,
            });
            let call = client.call(zeron_rpc::methods::UPLOAD_COMMIT, params);
            match tokio::time::timeout(TRANSFER_COMMIT_TIMEOUT, call).await {
                Err(_) => {
                    links.invalidate(target);
                    return Err(Transient("commit timed out; peer link suspect".into()));
                }
                Ok(Err(zeron_rpc::RpcError::Failed(err))) => {
                    return Err(Permanent(format!("host refused commit: {err}")));
                }
                Ok(Err(err)) => {
                    links.invalidate(target);
                    return Err(Transient(format!("commit failed: {err}")));
                }
                Ok(Ok(_)) => {}
            }
        }
        Ok(())
    }

    /// Upload a tool result's full output/diff to the R2 sidecar
    /// (`PUT {edge}/blob/{chatId}/{partId}[.diff]`, docs/chat2-sync.md A2).
    /// Fire-and-forget: the doc already carries the summary, so a lost upload
    /// degrades to "full output unavailable" — it must never block or fail
    /// the run. Offline/edge-less engines skip silently.
    pub fn upload_tool_sidecar(&self, chat_id: &str, payload: zeron_doc::SidecarPayload) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return; // bare sync callers (unit tests) skip rather than panic
        };
        let http = self.inner.http.clone();
        let base = format!(
            "{}/blob/{}/{}",
            edge.url.trim_end_matches('/'),
            chat_id,
            encode_part_segment(&payload.part_id)
        );
        self.spawn_worker_on(&runtime, async move {
            let Some(bearer) = edge.bearer().await else {
                return; // signed out; summary-only until the next session
            };
            let mut puts: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
            if let Some(output) = &payload.output {
                puts.push((
                    base.clone(),
                    "text/plain; charset=utf-8",
                    output.clone().into_bytes(),
                ));
            }
            if let Some(diff) = &payload.diff
                && let Ok(json) = serde_json::to_vec(diff)
            {
                puts.push((format!("{base}.diff"), "application/json", json));
            }
            for (url, content_type, body) in puts {
                let sent = http
                    .put(&url)
                    .bearer_auth(&bearer)
                    .header("content-type", content_type)
                    .body(body)
                    .send()
                    .await;
                match sent {
                    Ok(res) if res.status().is_success() => {}
                    Ok(res) => tracing::warn!(url, status = res.status().as_u16(),
                        "tool sidecar upload rejected"),
                    Err(err) => {
                        tracing::warn!(url, error = %err, "tool sidecar upload failed (best-effort)")
                    }
                }
            }
        });
    }

    /// Fetch a sidecar blob by its doc-resident ref (`{chatId}/{partId}` or
    /// `…​.diff`) — the UI's lazy "Show full output" path, served over RPC
    /// because the UI crate has no HTTP client or edge bearer.
    pub async fn fetch_tool_blob(&self, blob_ref: &str) -> Result<String, EngineError> {
        // Same shape `apply_sidecar_refs` writes; anything else is a forged ref.
        let valid = blob_ref.split_once('/').is_some_and(|(chat, part)| {
            !chat.is_empty()
                && chat.len() <= 128
                && chat
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && !part.is_empty()
                && part.len() <= 200
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:#~-".contains(&b))
        });
        if !valid {
            return Err(EngineError::Other(format!("bad blob ref: {blob_ref}")));
        }
        let Some(edge) = self.inner.config.edge.clone() else {
            return Err(EngineError::Other("offline: no edge configured".into()));
        };
        let Some(bearer) = edge.bearer().await else {
            return Err(EngineError::Other("signed out".into()));
        };
        // `valid` above guarantees the split; re-split to encode the part
        // segment for transport (PART_RE allows `#`, which a raw URL would
        // truncate as a fragment — the 2026-08-10 silent-collision bug).
        let (chat, part) = blob_ref.split_once('/').expect("validated above");
        let url = format!(
            "{}/blob/{}/{}",
            edge.url.trim_end_matches('/'),
            chat,
            encode_part_segment(part)
        );
        let res = self
            .inner
            .http
            .get(&url)
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("sidecar fetch failed: {e}")))?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "sidecar fetch: HTTP {}",
                res.status().as_u16()
            )));
        }
        res.text()
            .await
            .map_err(|e| EngineError::Other(format!("sidecar body read failed: {e}")))
    }

    /// §2.2 writer discipline: we host a chat iff its workspace row's `deviceId` is
    /// ours; a chat with no row is claimable (claim-on-first-command). Without a
    /// wired workspace host (bare-DocHost tests) every open chat is ours — M2's
    /// behavior, now the degenerate case.
    fn is_host(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|ws| ws.is_host(chat_id))
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    pub(crate) fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    /// The harness a request dispatches on: the request's own pick when it
    /// carries one (rides the command plane, immune to registry-row races),
    /// else [`Self::harness_for`].
    pub(crate) fn harness_for_request(
        &self,
        chat_id: &str,
        request: &zeron_proto::RunRequest,
    ) -> HarnessId {
        request.harness.unwrap_or_else(|| self.harness_for(chat_id))
    }

    /// Drain pending commands (host-only): evaluate → mark processed BEFORE execute →
    /// execute → write the outcome as the sole outcome writer.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.sessions() else {
            return; // executor not wired yet (or retired); the set_sessions kick re-drains
        };
        if !self.is_host(&handle.chat_id) {
            return;
        }
        // Entries this pass decided to leave alone (processed dedupe hits).
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            // Dead-command sweep: Pending in the doc, consumed by the ledger,
            // and NOT mid-execution in this process — the crash window
            // between mark-processed and the outcome write. Left alone it is
            // a send no drain or retry can ever reach ("Sending…" forever,
            // 2026-08-19); terminalize it so the truth lands in the doc and
            // a user retry can mint a fresh attempt.
            for c in &commands {
                if c.status == SessionCommandStatus::Pending
                    && !skipped.contains(&c.id)
                    && is_processed(&c.id)
                    && !lock(&self.inner.executing).contains(&c.id)
                {
                    tracing::warn!(chat = %handle.chat_id, command = %c.id,
                        "command consumed but never resolved (crash mid-execute?); rejecting");
                    self.resolve_command(
                        handle,
                        &c.id,
                        SessionCommandStatus::Rejected,
                        Some("interrupted before completion — retry to send again"),
                    );
                    skipped.insert(c.id.clone());
                }
            }
            let Some(entry) = commands
                .iter()
                .find(|c| {
                    c.status == SessionCommandStatus::Pending
                        && !skipped.contains(&c.id)
                        && !is_processed(&c.id)
                })
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            // Queued-attachment gate (BEFORE the processed mark — a deferred
            // command must stay eligible): a Run/Steer naming `pending://`
            // refs whose bytes haven't landed on this device yet waits for
            // the transfer instead of running without its images. The wait is
            // bounded; past it the command fails loudly.
            if matches!(disposition, CommandDisposition::Execute) {
                let missing = self.missing_attachments(&entry);
                if !missing.is_empty() {
                    if now_ms().saturating_sub(entry.issued_at) < ATTACHMENT_WAIT_MAX_MS {
                        tracing::info!(chat = %handle.chat_id, command = %entry.id,
                            missing = missing.len(), "command deferred: attachment bytes in transit");
                        self.arm_attachment_wait(handle);
                        return; // preserve order; UploadCommit / the wait timer re-kick
                    }
                    if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                        tracing::error!(chat = %handle.chat_id, error = %err,
                            "processed-ledger write failed; halting drain");
                        return;
                    }
                    tracing::warn!(chat = %handle.chat_id, command = %entry.id,
                        "command rejected: attachments never arrived");
                    self.resolve_command(
                        handle,
                        &entry.id,
                        SessionCommandStatus::Rejected,
                        Some("attachments never arrived"),
                    );
                    continue;
                }
            }
            // In-flight claim: guards the dead-command sweep (an id in
            // `executing` is alive, not crashed) and serializes racing
            // drains on the same entry.
            if !lock(&self.inner.executing).insert(entry.id.clone()) {
                skipped.insert(entry.id.clone());
                continue;
            }
            // Mark BEFORE executing: a crash mid-execution must never double-run a
            // command whose side effect may already have happened.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                lock(&self.inner.executing).remove(&entry.id);
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = match self.execute(&sessions, handle, &entry).await {
                        Ok(outcome) => outcome,
                        Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                    };
                    self.resolve_command(handle, &entry.id, status, resolution.as_deref());
                }
            }
            lock(&self.inner.executing).remove(&entry.id);
        }
    }

    /// The command's `pending://` attachment refs whose bytes are NOT on this
    /// device's disk yet. Empty when the command names none, when everything
    /// has landed, or when no uploads store is wired (tests) — absence of the
    /// subsystem must never wedge a queue.
    fn missing_attachments(&self, entry: &SessionCommandEntry) -> Vec<String> {
        let refs: Vec<String> = match &entry.payload {
            SessionCommandPayload::Run { request, .. } => request
                .attachments
                .iter()
                .filter(|p| crate::uploads::is_pending_ref(p))
                .cloned()
                .collect(),
            SessionCommandPayload::Steer { prompt, .. } => crate::uploads::pending_refs_in(prompt),
            _ => Vec::new(),
        };
        if refs.is_empty() {
            return refs;
        }
        let Some(uploads) = self.inner.uploads.get() else {
            return Vec::new();
        };
        refs.into_iter()
            .filter(|r| uploads.resolve_pending(r).is_none())
            .collect()
    }

    /// Arm (once per chat) the deferred-command re-check loop: while a
    /// pending unprocessed command still waits on attachment bytes, re-drain
    /// on a cadence so the bounded wait actually expires even if every
    /// event-driven kick was missed.
    fn arm_attachment_wait(&self, handle: &Arc<ChatDocHandle>) {
        let chat = handle.chat_id.clone();
        if !lock(&self.inner.drain_waiting).insert(chat.clone()) {
            return;
        }
        let weak = Arc::downgrade(handle);
        let host = self.clone();
        self.spawn_worker(async move {
            loop {
                tokio::time::sleep(ATTACHMENT_WAIT_RECHECK).await;
                let Some(handle) = weak.upgrade() else { break };
                if !host.awaiting_attachments(&handle) {
                    break;
                }
                host.drain_commands(&handle).await;
                let Some(handle) = weak.upgrade() else { break };
                if !host.awaiting_attachments(&handle) {
                    break;
                }
            }
            lock(&host.inner.drain_waiting).remove(&chat);
        });
    }

    /// True while some pending, unprocessed command still waits on bytes.
    fn awaiting_attachments(&self, handle: &Arc<ChatDocHandle>) -> bool {
        let commands = handle.doc.read_commands().unwrap_or_default();
        commands.iter().any(|c| {
            c.status == SessionCommandStatus::Pending
                && !self.inner.store.is_processed(&c.id).unwrap_or(false)
                && !self.missing_attachments(c).is_empty()
        })
    }

    /// Rewrite a request's landed `pending://` refs to this device's absolute
    /// paths — in the attachments list AND the prompt text — so the harness
    /// (and the persisted user entry) see ordinary local files, exactly like
    /// the legacy pre-upload flow produced.
    fn resolve_request_attachments(&self, request: &mut zeron_proto::RunRequest) {
        let Some(uploads) = self.inner.uploads.get() else {
            return;
        };
        for path in request.attachments.iter_mut() {
            if let Some(abs) = uploads.resolve_pending(path) {
                request.prompt = request.prompt.replace(path.as_str(), &abs);
                *path = abs;
            }
        }
    }

    /// [`Self::resolve_request_attachments`] for a bare prompt (Steer).
    fn resolve_prompt_attachments(&self, prompt: &str) -> String {
        let Some(uploads) = self.inner.uploads.get() else {
            return prompt.to_string();
        };
        let mut out = prompt.to_string();
        for r in crate::uploads::pending_refs_in(prompt) {
            if let Some(abs) = uploads.resolve_pending(&r) {
                out = out.replace(&r, &abs);
            }
        }
        out
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle
            .doc
            .set_command_status(command_id, status, resolution)
        {
            tracing::warn!(
                chat = %handle.chat_id,
                command = %command_id,
                error = %err,
                "command outcome write failed"
            );
        }
    }

    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                let mut request = request.clone();
                // Queued-attachment refs (`pending://`) resolve to this
                // host's absolute paths before anything persists or
                // dispatches — the drain already gated on the bytes being
                // present, so every ref resolves here.
                self.resolve_request_attachments(&mut request);
                // Worktree directive (WorktreeSpec): materialize on THIS host at
                // drain time — the durable command plane replaces the sender's
                // old blocking CreateWorktree relay RPC, whose lost reply wedged
                // the composer on "Sending…" while the run proceeded anyway.
                // `take()` resolves the request before dispatch, so the journal
                // and steer→new-turn fallbacks reuse the created path instead of
                // minting another checkout.
                let fresh_worktree = match request.worktree.take() {
                    Some(spec) => {
                        let (cwd, fresh) = self.materialize_worktree(chat_id, &spec).await?;
                        request.cwd = cwd;
                        fresh
                    }
                    None => None,
                };
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                    // A pre-existing row (the client's createChat raced ahead)
                    // still carries the repo folder — repoint it at the fresh
                    // worktree, and stamp the actual `zeron/<name>` branch so
                    // the footer and the title-rename flow see it.
                    if let Some(wt) = &fresh_worktree {
                        if let Err(err) = ws.set_chat_cwd(chat_id, &wt.path) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree cwd stamp failed");
                        }
                        if let Err(err) = ws.set_chat_branch(chat_id, &wt.branch) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree branch stamp failed");
                        }
                    }
                }
                let harness = self.harness_for_request(chat_id, &request);
                // A row with no config renders no harness glyph (and every
                // later dispatch falls back to the engine default), so stamp
                // what this run actually executes with. Claimed rows and
                // catalog-not-loaded createChats both land here; the racing
                // real createChat carries the same picked values.
                if let Some(ws) = self.workspace()
                    && ws.chat_config(chat_id).is_none()
                {
                    let config = zeron_proto::ChatConfig {
                        harness,
                        model: request.model.clone(),
                        reasoning: request.reasoning,
                        model_options: request.model_options.clone(),
                        sandbox: request.sandbox,
                    };
                    if let Err(err) = ws.set_chat_config(chat_id, &config) {
                        tracing::warn!(chat = %chat_id, error = %err, "run-config backfill failed");
                    }
                }
                // Timestamp canonicalization: the user message lands in
                // history at the moment the user SENT it (the entry's
                // issued_at, clamped against clock skew) — not whenever this
                // host got around to draining a queued command. Idempotent by
                // id, so the dispatch path's own execution-time write dedupes
                // to a no-op.
                if let Err(err) = handle.write_user_message(
                    message_id,
                    &request.prompt,
                    entry.issued_at.min(now_ms()),
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "canonical user-message write failed");
                }
                self.dispatch_with_source_context(
                    sessions,
                    chat_id,
                    harness,
                    request,
                    Some(message_id.clone()),
                )
                .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                // Same `pending://` → absolute rewrite as the Run arm.
                let prompt = &self.resolve_prompt_attachments(prompt);
                // Same send-time canonicalization as the Run arm.
                if let Some(message_id) = message_id
                    && let Err(err) =
                        handle.write_user_message(message_id, prompt, entry.issued_at.min(now_ms()))
                {
                    tracing::warn!(chat = %chat_id, error = %err, "canonical user-message write failed");
                }
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        // No live steerable run: the durable command still delivers —
                        // run it as the next turn (zeron's fallback, executor-side).
                        // After an engine restart `last_request` is empty too, so
                        // rebuild the run config from the chat's workspace row
                        // (zeron derived dispatch config from the chat row the
                        // same way — sessions.ts:601-620); dispatch's engine-owned
                        // resume then reattaches the prior harness conversation.
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt.clone();
                        request.resume = None; // dispatch re-derives the harness session
                        // A reused config must not re-inline the PREVIOUS
                        // turn's images; this steer's own refs (if any) already
                        // ride the prompt text.
                        request.attachments = Vec::new();
                        let harness = self.harness_for_request(chat_id, &request);
                        self.dispatch_with_source_context(
                            sessions,
                            chat_id,
                            harness,
                            request,
                            message_id.clone(),
                        )
                        .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                if sessions.respond_input(chat_id, request_id, answers.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                // No live resolver. Only a request id the doc shows as an
                // OPEN question on a SETTLED entry gets the orphan fallback:
                // a mismatched or already-resolved id is a stale/buggy answer
                // and must still reject, and a still-streaming entry's
                // question belongs to the live run (a just-consumed resolver
                // racing a second answer must not spawn a duplicate turn).
                let questions = handle.doc.read_entries().ok().and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .filter(|e| e.status != Some(MessageStatus::Streaming))
                        .find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Input {
                                    request_id: rid,
                                    questions,
                                    resolved: false,
                                    ..
                                } if rid == request_id => Some(questions.clone()),
                                _ => None,
                            })
                        })
                });
                let Some(questions) = questions else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request".into()),
                    ));
                };
                // The run died under the question (engine restart, crash).
                // The question is still open in the doc and the command is
                // durable, so honor it anyway — stamp the part resolved and
                // deliver the answers as the next (resumed) turn, the same
                // fallback a dead-run steer takes. The question UI stays up
                // until the user answers (user requirement); this is what
                // makes that answer still WORK.
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, ""));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request and no prior run config".into()),
                    ));
                };
                request.prompt = respond_input_prompt(&questions, answers);
                request.resume = None; // dispatch re-derives the harness session
                request.attachments = Vec::new();
                if let Err(err) = handle.doc.resolve_input(request_id) {
                    tracing::warn!(chat = %chat_id, request = %request_id, error = %err,
                        "orphaned input resolve failed");
                }
                let harness = self.harness_for_request(chat_id, &request);
                self.dispatch_with_source_context(sessions, chat_id, harness, request, None)
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("answered as new turn".into()),
                ))
            }
        }
    }

    async fn capture_source_context(&self, cwd: &str) -> Option<ConversationSourceContext> {
        let repos = self.inner.repos.get()?;
        let path = Path::new(cwd);
        let identity = repos.checkout_identity(path).await.ok()?;
        let branch = repos.current_branch(path).await.ok()?;
        let head_sha = repos.head_sha(path).await.ok().flatten();
        Some(ConversationSourceContext {
            checkout_id: identity.id,
            repo_root: identity.root.to_string_lossy().into_owned(),
            cwd: cwd.to_string(),
            branch,
            head_sha,
            observed_at: chrono::Utc::now(),
        })
    }

    /// Every fresh harness dispatch crosses this boundary, including
    /// steer/input fallbacks and crash recovery. Capture immediately before
    /// dispatch so the conversation records the checkout the harness will
    /// actually observe, rather than whichever checkout state was current on
    /// an earlier turn.
    pub(crate) async fn dispatch_with_source_context(
        &self,
        sessions: &SessionsEngine,
        chat_id: &str,
        harness: HarnessId,
        request: zeron_proto::RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        if let Some(workspace) = self.workspace()
            && let Some(context) = self.capture_source_context(&request.cwd).await
            && let Err(err) = workspace.set_chat_source_context(chat_id, &context)
        {
            tracing::warn!(chat = %chat_id, error = %err, "conversation source stamp failed");
        }
        sessions
            .dispatch(chat_id, harness, request, message_id)
            .await
    }

    /// Create (or reuse) the isolated worktree a Run's [`zeron_proto::WorktreeSpec`]
    /// asks for, returning the resolved cwd plus the fresh worktree when one was
    /// actually created. Reuse guard: a chat whose row already points inside a
    /// linked worktree of the same repo keeps it — a duplicate Run (client retry
    /// after a lost ack, ledger reset) must not mint a second checkout.
    async fn materialize_worktree(
        &self,
        chat_id: &str,
        spec: &zeron_proto::WorktreeSpec,
    ) -> Result<(String, Option<zeron_proto::Worktree>), EngineError> {
        if let Some(ws) = self.workspace()
            && let Ok(Some(chat)) = ws.chat(chat_id)
            && let Some(cwd) = chat.cwd
            && cwd != spec.repo_path
            && crate::workspace_host::linked_worktree_root(std::path::Path::new(&cwd)).as_deref()
                == Some(spec.repo_path.as_str())
        {
            tracing::info!(chat = %chat_id, cwd = %cwd, "worktree spec: reusing the chat's existing worktree");
            return Ok((cwd, None));
        }
        let repos = self
            .inner
            .repos
            .get()
            .ok_or_else(|| EngineError::Other("repos engine not wired".into()))?;
        let worktree = repos
            .create_worktree(std::path::Path::new(&spec.repo_path), &spec.base)
            .await?;
        tracing::info!(
            chat = %chat_id,
            path = %worktree.path,
            branch = %worktree.branch,
            "worktree materialized for run"
        );
        Ok((worktree.path.clone(), Some(worktree)))
    }

    /// A steer-turned-run with no in-process `last_request` (engine restarted
    /// since the last turn): rebuild the run config from the chat's workspace
    /// row — cwd from the row, model/reasoning/options/sandbox from its config
    /// (composer defaults otherwise). `None` without a workspace host or row.
    // (Also the RespondInput dead-run fallback's config source.)
    pub(crate) fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<zeron_proto::RunRequest> {
        let workspace = self.workspace()?;
        let chat = match workspace.chat(chat_id) {
            Ok(chat) => chat?,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                return None;
            }
        };
        let config = chat.config;
        Some(zeron_proto::RunRequest {
            prompt: prompt.to_string(),
            harness: config.as_ref().map(|c| c.harness),
            model: config.as_ref().and_then(|c| c.model.clone()),
            reasoning: config.as_ref().and_then(|c| c.reasoning),
            model_options: config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat.cwd.unwrap_or_default(),
            sandbox: config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(zeron_proto::SandboxLevel::WorkspaceWrite),
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
            worktree: None,
        })
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        if handle.retired.load(Ordering::Relaxed) {
            // A chat2 seed replaced this lineage on disk; persisting this
            // handle's fat doc would clobber the thin one. But retired with
            // NO thin lineage on disk (a stuck handle from the born-gen2
            // race) means this doc is its transcript's only copy — skipping
            // the save turned an app quit into total loss (2026-08-11);
            // persist it, and let the adopt path convert it on reopen.
            let thin_on_disk = matches!(
                self.inner.store.load_snapshot_with_cursor(&handle.chat_id),
                Ok(Some((_, _, epoch))) if epoch >= crate::chat2_host::CHAT2_DOC_EPOCH
            );
            if thin_on_disk {
                return;
            }
        }
        match handle.doc.export_snapshot() {
            Ok(bytes) => {
                handle.snapshot_bytes.store(bytes.len(), Ordering::Relaxed);
                if let Err(err) = self.inner.store.save_snapshot(&handle.chat_id, &bytes) {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot export failed");
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }

    /// Close all account-scoped room memberships before graceful engine
    /// draining. Auth-aware join supervisors will not install a late client.
    pub fn disconnect_edge(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            lock(&handle.chat2).take();
            lock(&handle.chat2_local_sub).take();
        }
    }
}

/// The resumed-turn prompt for answers to a question whose run died: each
/// answer paired with its question text so the reattached conversation reads
/// naturally. Pure.
pub fn respond_input_prompt(
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .map(|q| q.question.trim())
            .filter(|q| !q.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}

/// Percent-encode one URL path segment of a sidecar part id. PART_RE's
/// alphabet includes `#` and `:` — legal in R2 keys and doc refs, but a raw
/// `#` in a URL is a fragment delimiter (the request would silently hit the
/// truncated key, colliding parts). The Worker decodes before validating.
fn encode_part_segment(part_id: &str) -> String {
    let mut out = String::with_capacity(part_id.len());
    for byte in part_id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod transfer_progress_tests {
    use super::{DocHost, DocHostConfig, TransferProgressGuard};
    use std::sync::Arc;

    fn host() -> (tempfile::TempDir, DocHost) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(zeron_sync::DocsStore::open(dir.path()).expect("store opens"));
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "dev-test".into(),
                default_harness: zeron_proto::HarnessId::Mock,
                edge: None,
            },
        );
        (dir, host)
    }

    #[test]
    fn set_upserts_by_upload_id_and_clear_retires_only_its_entry() {
        let (_dir, host) = host();
        let rx = host.watch_transfers();
        assert!(rx.borrow().is_empty());

        host.transfer_progress_set("u1", "a.png", 0, 1_000);
        host.transfer_progress_set("u2", "b.png", 0, 400);
        host.transfer_progress_set("u1", "a.png", 300, 1_000);
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.len(), 2, "upsert must not duplicate u1");
        let u1 = snapshot.iter().find(|t| t.upload_id == "u1").unwrap();
        assert_eq!((u1.done, u1.total), (300, 1_000));

        host.transfer_progress_clear("u1");
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.len(), 1, "u2 must survive u1's retirement");
        assert_eq!(snapshot[0].upload_id, "u2");
    }

    #[test]
    fn guard_retires_the_entry_on_every_exit_path() {
        let (_dir, host) = host();
        let rx = host.watch_transfers();
        host.transfer_progress_set("u1", "a.png", 0, 9);
        {
            let _guard = TransferProgressGuard {
                host: &host,
                upload_id: "u1",
            };
            assert_eq!(rx.borrow().len(), 1);
            // An early return / error propagation drops the guard here.
        }
        assert!(
            rx.borrow().is_empty(),
            "a failed attempt must not leave a phantom ring behind"
        );
    }

    #[test]
    fn late_subscriber_sees_current_set_first() {
        let (_dir, host) = host();
        host.transfer_progress_set("u1", "a.png", 750, 1_000);
        // watch_stream's contract: current value first, then changes — a UI
        // attaching mid-transfer must render the ring immediately.
        let rx = host.watch_transfers();
        assert_eq!(rx.borrow().len(), 1);
        assert_eq!(rx.borrow()[0].done, 750);
    }
}

#[cfg(test)]
mod source_context_tests {
    use super::{DocHost, DocHostConfig};
    use std::process::Command;
    use std::sync::Arc;

    fn git(repo: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn capture_source_context_reads_the_dispatch_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "feature/captured"]);
        git(&repo, &["config", "user.name", "Zeron Test"]);
        git(&repo, &["config", "user.email", "zeron@example.com"]);
        std::fs::write(repo.join("README.md"), "capture\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "capture"]);

        let store =
            Arc::new(zeron_sync::DocsStore::open(dir.path().join("docs")).expect("store opens"));
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: zeron_proto::HarnessId::Mock,
                edge: None,
            },
        );
        host.set_repos(crate::repos::Repos::new(
            &dir.path().join("data"),
            "device-a",
        ));

        let context = host
            .capture_source_context(repo.to_str().unwrap())
            .await
            .expect("source context");
        assert_eq!(context.branch, "feature/captured");
        assert_eq!(context.cwd, repo.to_string_lossy());
        assert_eq!(
            context.repo_root,
            repo.canonicalize().unwrap().to_string_lossy()
        );
        assert!(context.head_sha.is_some());
        assert!(!context.checkout_id.is_empty());
    }
}

#[cfg(test)]
mod degrade_grace_tests {
    use super::{DEGRADE_GRACE, DegradeGrace, GraceKey};
    use std::time::{Duration, Instant};

    #[test]
    fn blips_shorter_than_the_grace_never_report() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        // A 300ms room join: degraded at t0, healthy again shortly after.
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0));
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0 + Duration::from_millis(300)));
        assert!(!g.degraded(GraceKey::Chat("c1"), false, t0 + Duration::from_millis(600)));
        // The recovery cleared the timer — a fresh blip starts from zero.
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn persistent_degradation_reports_after_the_grace_and_clears_instantly() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        assert!(!g.degraded(GraceKey::Registry, true, t0));
        assert!(!g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE / 2));
        assert!(g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE));
        assert!(g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE * 3));
        // Hide-fast: one healthy sample reports Connected immediately.
        assert!(!g.degraded(GraceKey::Registry, false, t0 + DEGRADE_GRACE * 4));
    }

    #[test]
    fn sources_are_independent_and_closed_chats_are_dropped() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        assert!(!g.degraded(GraceKey::Chat("gone"), true, t0));
        assert!(!g.degraded(GraceKey::OsPath, true, t0));
        // The chat's doc closes; its timer must not leak.
        g.retain_chats(|id| id != "gone");
        assert!(g.chats.is_empty());
        // OsPath kept its own timer through the retain.
        assert!(g.degraded(GraceKey::OsPath, true, t0 + DEGRADE_GRACE));
    }
}

#[cfg(test)]
mod part_segment_tests {
    use super::encode_part_segment;

    #[test]
    fn hash_and_colon_are_escaped_unreserved_pass_through() {
        assert_eq!(encode_part_segment("m1#c1"), "m1%23c1");
        assert_eq!(encode_part_segment("tool:call_9"), "tool%3Acall_9");
        assert_eq!(encode_part_segment("plain-id_0.diff~"), "plain-id_0.diff~");
    }
}

/// Per-chat background task: reacts to doc changes (local commits and remote imports)
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                handle.publish_messages_if_watched();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
                // chat2 host duties ride the same quiesce tick (C3):
                // threshold checkpoints + the tail sidecar publish.
                host.chat2_maintenance(&handle).await;
                // Post-quiesce eviction pass: sizes just refreshed.
                host.evict_over_budget();
            }
        }
    }
}
