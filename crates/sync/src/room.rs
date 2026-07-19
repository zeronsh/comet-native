//! `RoomClient` — a loro-protocol room client over WebSocket, speaking to the
//! TS edge's SessionRoom Durable Object (`edge/src/session-room.ts`).
//!
//! Wire format (loro-protocol 0.3, identical bytes to the npm package the edge
//! imports): every frame is `4-byte CRDT magic ("%LOR"/"%EPH"/…), varbytes
//! roomId, 1-byte message type, payload`. The messages this client exchanges:
//!
//! - `JoinRequest {auth, version}` → `JoinResponseOk {permission, version}` /
//!   `JoinError {code, message}` — version bytes are Loro `VersionVector`
//!   encodings; the server backfills `export({mode:"update", from: clientVV})`
//!   or a full snapshot when the client VV is empty/garbled.
//! - `DocUpdate {updates[], batchId}` acknowledged by `Ack {refId, status}`.
//! - `DocUpdateFragmentHeader {batchId, fragmentCount, totalSizeBytes}` +
//!   `DocUpdateFragment {batchId, index, fragment}` for payloads above the
//!   256KB message cap (the edge fragments at 200_000 payload bytes).
//! - `RoomError {RejoinSuggested | Evicted}`, `Leave`.
//!
//! Sync discipline (mirrors the edge's expectations):
//! - On (re)join, the server's `JoinResponseOk.version` is used to export and
//!   push everything the server lacks — this doubles as resend-after-reconnect
//!   (unacked local commits are re-derived from the doc, never queued).
//! - `Ack{InvalidUpdate}` is the §3.1 stale-peer signal (import concurrent to
//!   a shallow-snapshot trim): the client rejoins on the same socket to resync
//!   fresh, then re-submits from the server's VV.
//! - `Ack{FragmentTimeout}` (reassembly state lost to DO hibernation): the
//!   whole batch is resent.
//! - Presence rides the `%EPH` sub-room as `loro::awareness::EphemeralStore`
//!   payloads relayed verbatim.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use loro::awareness::EphemeralStore;
use loro::{ExportMode, LoroDoc, VersionVector};
use loro_protocol::{
    BatchId, CrdtType, JoinErrorCode, Permission, ProtocolMessage, RoomErrorCode, UpdateStatusCode,
    decode, encode,
};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Payload bytes per outbound fragment — mirrors the edge's `FRAGMENT_BYTES`
/// (leaves envelope room under loro-protocol's 256KB message cap).
const FRAGMENT_BYTES: usize = 200_000;
/// Refuse absurd inbound fragment batches (a healthy backfill snapshot is MBs).
const MAX_REASSEMBLED_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAGMENT_COUNT: u64 = 16 * 1024;
/// Presence timeout, matching the edge's `new EphemeralStore(30_000)`.
const EPHEMERAL_TIMEOUT_MS: i64 = 30_000;
/// Text `"ping"` keepalive interval — answered by the DO's hibernation-safe
/// auto-response pair without waking it.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Stop resubmitting after this many InvalidUpdate-triggered rejoins in one
/// session — our history predates the room's shallow start and can never
/// import; recovery is an app-layer concern (§3.1).
const MAX_INVALID_REJOINS: u32 = 3;

/// Errors surfaced by [`RoomClient`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum SyncError {
    #[error("websocket: {0}")]
    WebSocket(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("join refused: {0}")]
    JoinRefused(String),
    #[error("loro: {0}")]
    Loro(String),
    #[error("client is shut down")]
    Closed,
}

/// Connection/sync lifecycle notifications (best-effort broadcast; receivers
/// may lag and miss intermediate events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomEvent {
    /// Joined (or re-joined) the room; backfill and resubmission are underway.
    Connected,
    /// The connection dropped; the client is backing off before reconnecting.
    Disconnected,
    /// Remote loro updates were imported into the doc.
    RemoteUpdate,
    /// Remote ephemeral (presence) state was applied.
    EphemeralUpdate,
    /// The server evicted us; the client will NOT reconnect.
    Evicted,
}

/// A byte-frame duplex to the room: `tx` outbound, `rx` inbound. Closing
/// either side ends the session.
pub(crate) struct Pipe {
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
    pub(crate) rx: mpsc::Receiver<Vec<u8>>,
}

/// Dials one connection attempt. The production impl speaks WebSocket; tests
/// substitute an in-memory duplex.
pub(crate) trait Connector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>>;
}

struct WsConnector {
    url: String,
}

impl Connector for WsConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
        let url = self.url.clone();
        Box::pin(async move {
            let (ws, _) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            tokio::spawn(pump(ws, out_rx, in_tx));
            Ok(Pipe { tx: out_tx, rx: in_rx })
        })
    }
}

/// Shuttle frames between the WebSocket and the actor's channels, plus the
/// text-ping keepalive. Ends (dropping `in_tx`, which the actor observes) when
/// either side closes.
async fn pump(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => {
                    // Actor is done (shutdown): close politely.
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    if in_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {} // text "pong" / control frames
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// A live room membership for one Loro doc.
///
/// Owns a background task that keeps `doc` converged with the room: pushes
/// local commits (via `subscribe_local_update`), imports remote updates and
/// backfill, relays `%EPH` presence, reassembles/produces fragments, and
/// reconnects with exponential backoff after connection loss. Dropping the
/// client aborts the task immediately; [`RoomClient::shutdown`] leaves the
/// room cleanly first.
pub struct RoomClient {
    doc: LoroDoc,
    eph: EphemeralStore,
    events: broadcast::Sender<RoomEvent>,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Doc + ephemeral local-update subscriptions (drop = unsubscribe).
    _subs: Vec<loro::Subscription>,
}

impl RoomClient {
    /// Connect to a loro-protocol room and keep `doc` in sync with it.
    ///
    /// `url` is the full, already-authenticated WebSocket URL (the edge takes
    /// the bearer as `?token=`, e.g. `wss://…/session/{chatId}/ws?token=…`);
    /// `room_id` is the doc room name carried inside the protocol frames (the
    /// chatId, or `ws/{orgId}` for workspace docs).
    ///
    /// Resolves once the initial join handshake succeeds — the JoinRequest
    /// carries the doc's version vector, and the server's backfill (updates or
    /// a full snapshot) is imported as it arrives. A first-attempt failure
    /// (unreachable edge, `JoinError`) is returned as `Err`; only after a
    /// successful join does the client keep reconnecting in the background.
    pub async fn connect(url: &str, room_id: &str, doc: LoroDoc) -> Result<Self, SyncError> {
        let connector = Arc::new(WsConnector { url: url.to_string() });
        Self::connect_with(connector, room_id, doc).await
    }

    pub(crate) async fn connect_with(
        connector: Arc<dyn Connector>,
        room_id: &str,
        doc: LoroDoc,
    ) -> Result<Self, SyncError> {
        let eph = EphemeralStore::new(EPHEMERAL_TIMEOUT_MS);

        let (local_tx, local_rx) = mpsc::unbounded_channel();
        let sub_doc = doc.subscribe_local_update(Box::new(move |bytes: &Vec<u8>| {
            let _ = local_tx.send(bytes.clone());
            true
        }));
        let (eph_tx, eph_rx) = mpsc::unbounded_channel();
        let sub_eph = eph.subscribe_local_updates(Box::new(move |bytes: &Vec<u8>| {
            let _ = eph_tx.send(bytes.clone());
            true
        }));

        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();

        let actor = RoomActor {
            doc: doc.clone(),
            eph: eph.clone(),
            room_id: room_id.to_string(),
            connector,
            local_rx,
            eph_rx,
            events: events.clone(),
            shutdown: shutdown_rx,
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                doc,
                eph,
                events,
                shutdown: shutdown_tx,
                task: Some(task),
                _subs: vec![sub_doc, sub_eph],
            }),
            Ok(Err(err)) => {
                task.abort();
                Err(err)
            }
            Err(_) => {
                task.abort();
                Err(SyncError::Closed)
            }
        }
    }

    /// The synced doc handle (reference clone of the one passed to `connect`).
    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    /// Presence store relayed through the room's `%EPH` channel: `set` keys
    /// here to publish, read/subscribe to observe remote peers.
    pub fn ephemeral(&self) -> &EphemeralStore {
        &self.eph
    }

    /// Subscribe to connection/sync lifecycle events.
    pub fn events(&self) -> broadcast::Receiver<RoomEvent> {
        self.events.subscribe()
    }

    /// Leave the room (protocol `Leave` frames + close handshake) and stop the
    /// background task.
    pub async fn shutdown(mut self) -> Result<(), SyncError> {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), task).await.is_err() {
                abort.abort();
            }
        }
        Ok(())
    }
}

impl Drop for RoomClient {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ── background actor ────────────────────────────────────────────────────────

struct RoomActor {
    doc: LoroDoc,
    eph: EphemeralStore,
    room_id: String,
    connector: Arc<dyn Connector>,
    local_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    eph_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    events: broadcast::Sender<RoomEvent>,
    shutdown: watch::Receiver<bool>,
}

enum SessionEnd {
    /// Clean shutdown requested; Leave was sent.
    Shutdown,
    /// Fatal refusal (JoinError / RoomError::Evicted) — do not reconnect.
    Evicted(String),
    /// Connection failed or dropped — reconnect with backoff.
    Lost(SyncError),
}

impl RoomActor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let (end, joined) = match self.connector.connect().await {
                Ok(pipe) => self.run_session(pipe, &mut ready).await,
                Err(err) => (SessionEnd::Lost(err), false),
            };
            match end {
                SessionEnd::Shutdown => return,
                SessionEnd::Evicted(reason) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(SyncError::JoinRefused(reason)));
                    } else {
                        tracing::warn!(room = %self.room_id, %reason, "evicted from room");
                        let _ = self.events.send(RoomEvent::Evicted);
                    }
                    return;
                }
                SessionEnd::Lost(err) => {
                    if let Some(tx) = ready.take() {
                        // Never joined: fail `connect()` fast instead of
                        // silently retrying in the background.
                        let _ = tx.send(Err(err));
                        return;
                    }
                    tracing::warn!(room = %self.room_id, error = %err, "room connection lost");
                    let _ = self.events.send(RoomEvent::Disconnected);
                }
            }
            if joined {
                backoff = BACKOFF_BASE;
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.shutdown.changed() => return,
            }
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }

    /// Drive one connection until it ends. Returns the end reason and whether
    /// the session ever completed a join (for backoff reset).
    async fn run_session(
        &mut self,
        mut pipe: Pipe,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> (SessionEnd, bool) {
        // Local updates queued while disconnected are already in the doc; the
        // VV diff pushed on join re-derives them, so stale queue entries are
        // dropped rather than replayed.
        while self.local_rx.try_recv().is_ok() {}
        while self.eph_rx.try_recv().is_ok() {}

        let mut sess = Session {
            doc: self.doc.clone(),
            eph: self.eph.clone(),
            room_id: self.room_id.clone(),
            tx: pipe.tx.clone(),
            events: self.events.clone(),
            pending: HashMap::new(),
            fragments: HashMap::new(),
            joined_lor: false,
            joined_eph: false,
            invalid_rejoins: 0,
            full_resync_requested: false,
        };

        let version = sess.local_version_bytes();
        if let Err(err) = sess.send_join_loro(version).await {
            return (SessionEnd::Lost(err), false);
        }

        let end = loop {
            tokio::select! {
                _ = self.shutdown.changed() => {
                    let _ = sess
                        .send(&ProtocolMessage::Leave {
                            crdt: CrdtType::Loro,
                            room_id: sess.room_id.clone(),
                        })
                        .await;
                    if sess.joined_eph {
                        let _ = sess
                            .send(&ProtocolMessage::Leave {
                                crdt: CrdtType::LoroEphemeralStore,
                                room_id: sess.room_id.clone(),
                            })
                            .await;
                    }
                    break SessionEnd::Shutdown;
                }
                frame = pipe.rx.recv() => match frame {
                    None => break SessionEnd::Lost(SyncError::WebSocket("connection closed".into())),
                    Some(bytes) => match sess.handle_frame(&bytes, ready).await {
                        Ok(None) => {}
                        Ok(Some(end)) => break end,
                        Err(err) => break SessionEnd::Lost(err),
                    },
                },
                update = self.local_rx.recv() => match update {
                    None => break SessionEnd::Shutdown, // client dropped
                    // When not yet joined: covered by the join-time VV diff.
                    Some(update) => {
                        if sess.joined_lor
                            && let Err(err) = sess.send_loro_updates(vec![update]).await
                        {
                            break SessionEnd::Lost(err);
                        }
                    }
                },
                update = self.eph_rx.recv() => match update {
                    None => break SessionEnd::Shutdown,
                    // When not yet joined: presence is ephemeral; dropped by design.
                    Some(update) => {
                        if sess.joined_eph
                            && let Err(err) = sess.send_eph_updates(vec![update]).await
                        {
                            break SessionEnd::Lost(err);
                        }
                    }
                },
            }
        };
        let joined = sess.joined_lor;
        (end, joined)
    }
}

// ── per-connection protocol session ─────────────────────────────────────────

struct FragmentBuffer {
    crdt: CrdtType,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    total_size: usize,
}

struct Session {
    doc: LoroDoc,
    eph: EphemeralStore,
    room_id: String,
    tx: mpsc::Sender<Vec<u8>>,
    events: broadcast::Sender<RoomEvent>,
    /// Sent-but-unacked outbound batches, kept for FragmentTimeout resends.
    pending: HashMap<BatchId, Vec<Vec<u8>>>,
    /// Inbound reassembly buffers.
    fragments: HashMap<BatchId, FragmentBuffer>,
    joined_lor: bool,
    joined_eph: bool,
    invalid_rejoins: u32,
    full_resync_requested: bool,
}

impl Session {
    fn local_version_bytes(&self) -> Vec<u8> {
        let vv = self.doc.oplog_vv();
        // Empty bytes ask the server for a full snapshot (its fresh-doc path).
        if vv.is_empty() { Vec::new() } else { vv.encode() }
    }

    async fn send(&self, message: &ProtocolMessage) -> Result<(), SyncError> {
        let bytes = encode(message).map_err(SyncError::Protocol)?;
        self.tx
            .send(bytes)
            .await
            .map_err(|_| SyncError::WebSocket("connection closed".into()))
    }

    async fn send_join_loro(&self, version: Vec<u8>) -> Result<(), SyncError> {
        // Auth rides the URL (`?token=`); the frame-level auth field is unused
        // by the edge.
        self.send(&ProtocolMessage::JoinRequest {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            auth: Vec::new(),
            version,
        })
        .await
    }

    async fn handle_frame(
        &mut self,
        bytes: &[u8],
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> Result<Option<SessionEnd>, SyncError> {
        let message = decode(bytes).map_err(SyncError::Protocol)?;
        match message {
            ProtocolMessage::JoinResponseOk { crdt, version, permission, .. } => {
                self.on_join_ok(crdt, version, permission, ready).await?;
                Ok(None)
            }
            ProtocolMessage::JoinError { crdt, code, message, .. } => {
                if crdt == CrdtType::Loro {
                    if code == JoinErrorCode::VersionUnknown {
                        // Server can't diff from our VV — fall back to a full
                        // snapshot backfill.
                        self.send_join_loro(Vec::new()).await?;
                        return Ok(None);
                    }
                    return Ok(Some(SessionEnd::Evicted(format!("{code:?}: {message}"))));
                }
                tracing::warn!(room = %self.room_id, ?code, %message, "ephemeral join failed");
                Ok(None)
            }
            ProtocolMessage::DocUpdate { crdt, updates, .. } => {
                self.apply_remote(crdt, updates).await?;
                Ok(None)
            }
            ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                batch_id,
                fragment_count,
                total_size_bytes,
                ..
            } => {
                if fragment_count == 0
                    || fragment_count > MAX_FRAGMENT_COUNT
                    || total_size_bytes as usize > MAX_REASSEMBLED_BYTES
                {
                    tracing::warn!(
                        room = %self.room_id,
                        fragment_count,
                        total_size_bytes,
                        "rejecting oversized fragment batch"
                    );
                    return Ok(None);
                }
                self.fragments.insert(
                    batch_id,
                    FragmentBuffer {
                        crdt,
                        parts: vec![None; fragment_count as usize],
                        received: 0,
                        total_size: total_size_bytes as usize,
                    },
                );
                Ok(None)
            }
            ProtocolMessage::DocUpdateFragment { batch_id, index, fragment, .. } => {
                self.on_fragment(batch_id, index, fragment).await?;
                Ok(None)
            }
            ProtocolMessage::Ack { crdt, ref_id, status, .. } => {
                self.on_ack(crdt, ref_id, status).await?;
                Ok(None)
            }
            ProtocolMessage::RoomError { code, message, .. } => match code {
                RoomErrorCode::Evicted => {
                    Ok(Some(SessionEnd::Evicted(format!("RoomError: {message}"))))
                }
                _ => {
                    // RejoinSuggested (or unknown): refresh both sub-rooms on
                    // this socket.
                    let version = self.local_version_bytes();
                    self.send_join_loro(version).await?;
                    Ok(None)
                }
            },
            // Server never sends these to us; ignore.
            ProtocolMessage::JoinRequest { .. } | ProtocolMessage::Leave { .. } => Ok(None),
        }
    }

    async fn on_join_ok(
        &mut self,
        crdt: CrdtType,
        version: Vec<u8>,
        _permission: Permission,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> Result<(), SyncError> {
        match crdt {
            CrdtType::Loro => {
                self.joined_lor = true;
                // Resubmit-from-VV: push everything the server lacks. This
                // covers both fresh docs (first upload) and updates that went
                // unacked across a reconnect or stale-peer resync.
                if !self.doc.oplog_vv().is_empty() && self.invalid_rejoins < MAX_INVALID_REJOINS {
                    let server_vv = if version.is_empty() {
                        VersionVector::default()
                    } else {
                        VersionVector::decode(&version).unwrap_or_default()
                    };
                    let missing = self
                        .doc
                        .export(ExportMode::updates(&server_vv))
                        .map_err(|e| SyncError::Loro(e.to_string()))?;
                    if !missing.is_empty() {
                        self.send_loro_updates(vec![missing]).await?;
                    }
                }
                // Join presence once the doc room is up.
                self.send(&ProtocolMessage::JoinRequest {
                    crdt: CrdtType::LoroEphemeralStore,
                    room_id: self.room_id.clone(),
                    auth: Vec::new(),
                    version: Vec::new(),
                })
                .await?;
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
                let _ = self.events.send(RoomEvent::Connected);
            }
            CrdtType::LoroEphemeralStore => {
                self.joined_eph = true;
                let all = self.eph.encode_all();
                if !all.is_empty() {
                    self.send_eph_updates(vec![all]).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn apply_remote(
        &mut self,
        crdt: CrdtType,
        updates: Vec<Vec<u8>>,
    ) -> Result<(), SyncError> {
        match crdt {
            CrdtType::Loro => {
                let mut imported = false;
                for update in updates {
                    if update.is_empty() {
                        continue;
                    }
                    match self.doc.import(&update) {
                        Ok(_) => imported = true,
                        Err(err) => {
                            tracing::warn!(room = %self.room_id, error = %err, "remote update import failed");
                            if !self.full_resync_requested {
                                // Ask for a full snapshot backfill once; import
                                // of a snapshot merges, so this heals gaps.
                                self.full_resync_requested = true;
                                self.send_join_loro(Vec::new()).await?;
                            }
                        }
                    }
                }
                if imported {
                    let _ = self.events.send(RoomEvent::RemoteUpdate);
                }
            }
            CrdtType::LoroEphemeralStore => {
                let mut applied = false;
                for update in updates {
                    if update.is_empty() {
                        continue;
                    }
                    match self.eph.apply(&update) {
                        Ok(()) => applied = true,
                        Err(err) => {
                            tracing::warn!(room = %self.room_id, error = %err, "ephemeral apply failed");
                        }
                    }
                }
                if applied {
                    let _ = self.events.send(RoomEvent::EphemeralUpdate);
                }
            }
            other => {
                tracing::warn!(room = %self.room_id, ?other, "update for unsupported crdt");
            }
        }
        Ok(())
    }

    async fn on_fragment(
        &mut self,
        batch_id: BatchId,
        index: u64,
        fragment: Vec<u8>,
    ) -> Result<(), SyncError> {
        let Some(buffer) = self.fragments.get_mut(&batch_id) else {
            // Header never seen (or batch rejected) — nothing to assemble;
            // unlike the DO we hold no durable state, so just drop it.
            return Ok(());
        };
        let index = index as usize;
        if index >= buffer.parts.len() {
            self.fragments.remove(&batch_id);
            return Ok(());
        }
        if buffer.parts[index].is_none() {
            buffer.received += 1;
        }
        buffer.parts[index] = Some(fragment);
        if buffer.received < buffer.parts.len() {
            return Ok(());
        }
        let Some(buffer) = self.fragments.remove(&batch_id) else {
            return Ok(());
        };
        let mut total = Vec::with_capacity(buffer.total_size);
        for part in buffer.parts.into_iter().flatten() {
            total.extend_from_slice(&part);
        }
        self.apply_remote(buffer.crdt, vec![total]).await
    }

    async fn on_ack(
        &mut self,
        crdt: CrdtType,
        ref_id: BatchId,
        status: UpdateStatusCode,
    ) -> Result<(), SyncError> {
        match status {
            UpdateStatusCode::Ok => {
                self.pending.remove(&ref_id);
            }
            UpdateStatusCode::FragmentTimeout => {
                // DO hibernated mid-batch and lost reassembly state — resend
                // the whole batch (self-healing per the edge's design).
                if let Some(batch) = self.pending.remove(&ref_id) {
                    self.send_loro_updates(batch).await?;
                }
            }
            UpdateStatusCode::InvalidUpdate | UpdateStatusCode::PermissionDenied => {
                self.pending.remove(&ref_id);
                if crdt == CrdtType::Loro {
                    if self.invalid_rejoins >= MAX_INVALID_REJOINS {
                        tracing::error!(
                            room = %self.room_id,
                            "updates repeatedly rejected (stale peer past shallow start); giving up resubmission"
                        );
                        return Ok(());
                    }
                    self.invalid_rejoins += 1;
                    // §3.1 stale peer: resync fresh (rejoin with our VV pulls
                    // the server's post-trim state), then the JoinResponseOk
                    // handler resubmits from the server's VV.
                    let version = self.local_version_bytes();
                    self.send_join_loro(version).await?;
                } else {
                    tracing::warn!(room = %self.room_id, ?crdt, ?status, "update rejected");
                }
            }
            UpdateStatusCode::PayloadTooLarge => {
                self.pending.remove(&ref_id);
                tracing::error!(room = %self.room_id, "server rejected update as too large");
            }
            other => {
                self.pending.remove(&ref_id);
                tracing::warn!(room = %self.room_id, ?other, "unexpected ack status");
            }
        }
        Ok(())
    }

    /// Send loro updates, batching small ones and fragmenting any single
    /// update above the protocol payload budget. Every batch is tracked in
    /// `pending` until its Ack.
    async fn send_loro_updates(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let mut small: Vec<Vec<u8>> = Vec::new();
        let mut small_bytes = 0usize;
        for update in updates {
            if update.is_empty() {
                continue;
            }
            if update.len() > FRAGMENT_BYTES {
                self.send_fragmented(update).await?;
                continue;
            }
            if small_bytes + update.len() > FRAGMENT_BYTES {
                self.flush_small_batch(std::mem::take(&mut small)).await?;
                small_bytes = 0;
            }
            small_bytes += update.len();
            small.push(update);
        }
        if !small.is_empty() {
            self.flush_small_batch(small).await?;
        }
        Ok(())
    }

    async fn flush_small_batch(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let batch_id = new_batch_id();
        self.pending.insert(batch_id, updates.clone());
        self.send(&ProtocolMessage::DocUpdate {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            updates,
            batch_id,
        })
        .await
    }

    async fn send_fragmented(&mut self, update: Vec<u8>) -> Result<(), SyncError> {
        let batch_id = new_batch_id();
        self.pending.insert(batch_id, vec![update.clone()]);
        let fragment_count = update.len().div_ceil(FRAGMENT_BYTES);
        self.send(&ProtocolMessage::DocUpdateFragmentHeader {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            batch_id,
            fragment_count: fragment_count as u64,
            total_size_bytes: update.len() as u64,
        })
        .await?;
        for (index, chunk) in update.chunks(FRAGMENT_BYTES).enumerate() {
            self.send(&ProtocolMessage::DocUpdateFragment {
                crdt: CrdtType::Loro,
                room_id: self.room_id.clone(),
                batch_id,
                index: index as u64,
                fragment: chunk.to_vec(),
            })
            .await?;
        }
        Ok(())
    }

    async fn send_eph_updates(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let updates: Vec<Vec<u8>> = updates.into_iter().filter(|u| !u.is_empty()).collect();
        if updates.is_empty() {
            return Ok(());
        }
        // Presence payloads are tiny; no fragmentation or resend tracking.
        self.send(&ProtocolMessage::DocUpdate {
            crdt: CrdtType::LoroEphemeralStore,
            room_id: self.room_id.clone(),
            updates,
            batch_id: new_batch_id(),
        })
        .await
    }
}

fn new_batch_id() -> BatchId {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[..8]);
    BatchId(id)
}

#[cfg(test)]
mod tests;
